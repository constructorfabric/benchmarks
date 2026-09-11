//! The data plane: one proxy request, end to end (DESIGN §3.2 "Data Plane",
//! ADR 0001, ADR 0006).
//!
//! Order of operations per request:
//!
//! 1. CORS preflight short-circuit (before resolution, before tenant checks).
//! 2. `X-OAGW-Target-Host` routing header validation.
//! 3. Alias resolution + route matching (Control Plane).
//! 4. Endpoint selection (explicit target host, or round-robin).
//! 5. Rate limit.
//! 6. CORS validation of the actual cross-origin request.
//! 7. Auth plugin → guards → request transforms.
//! 8. Upstream call, then response transforms / guards, and the response back.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http::uri::Scheme;
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use hyper::upgrade::OnUpgrade;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};

use crate::config::OagwConfig;
use crate::domain::alias::{self, AliasDerivation};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{Endpoint, EndpointScheme, PathSuffixMode, PluginBinding};
use crate::domain::plugin::{ErrorContext, PluginConfig, RequestContext, ResponseContext};
use crate::domain::services::management::{ControlPlaneService, ProxyMethod, ResolvedTarget};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::cors;
use crate::infra::proxy::headers;
use crate::infra::proxy::rate_limit::{self, RateLimiter, RateScopeContext};

/// Body type every response uses, so streaming and buffered responses share one
/// handler signature.
pub type ProxyBody = axum::body::Body;

/// One inbound proxy request, already detached from the HTTP server.
#[derive(Debug)]
pub struct ProxyCall {
    /// Calling tenant.
    pub tenant_id: String,
    /// Authenticated subject, when known.
    pub user_id: Option<String>,
    /// Client IP, when known.
    pub client_ip: Option<String>,
    /// Request method.
    pub method: Method,
    /// The path suffix after `/proxy/{alias}` — empty or `/something`.
    pub path: String,
    /// Raw query string.
    pub query: String,
    /// Inbound headers, verbatim.
    pub headers: HeaderMap,
    /// Buffered request body (already size-checked).
    pub body: Bytes,
    /// The pending WebSocket upgrade, captured by the caller before responding.
    pub upgrade: Option<OnUpgrade>,
}

/// Executes proxy requests.
pub struct DataPlaneService {
    control_plane: Arc<ControlPlaneService>,
    client: Client<HttpsConnector<HttpConnector>, http_body_util::Full<Bytes>>,
    config: OagwConfig,
    auth: AuthPluginRegistry,
    guards: GuardPluginRegistry,
    transforms: TransformPluginRegistry,
    limiter: RateLimiter,
    round_robin: AtomicUsize,
}

impl DataPlaneService {
    /// Builds a data plane over a control plane.
    ///
    /// # Errors
    /// Returns [`DomainError::Internal`] when the TLS trust roots cannot be
    /// loaded.
    #[allow(clippy::duration_suboptimal_units)] // `Duration::from_mins` is unstable
    pub fn new(
        control_plane: Arc<ControlPlaneService>,
        config: OagwConfig,
    ) -> Result<Self, DomainError> {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|e| DomainError::Internal(format!("tls trust store unavailable: {e}")))?
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .timer(TokioTimer::new())
            .build(connector);
        Ok(Self {
            control_plane,
            client,
            config,
            auth: AuthPluginRegistry::empty(),
            guards: GuardPluginRegistry::empty(),
            transforms: TransformPluginRegistry::empty(),
            limiter: RateLimiter::new(),
            round_robin: AtomicUsize::new(0),
        })
    }

    /// Installs the plugin registries built by [`crate::infra::plugin::registry`].
    #[must_use]
    pub fn with_registries(
        mut self,
        auth: AuthPluginRegistry,
        guards: GuardPluginRegistry,
        transforms: TransformPluginRegistry,
    ) -> Self {
        self.auth = auth;
        self.guards = guards;
        self.transforms = transforms;
        self
    }

    /// The control plane this data plane reads from.
    #[must_use]
    pub fn control_plane(&self) -> &Arc<ControlPlaneService> {
        &self.control_plane
    }

    /// Runs one proxy request.
    ///
    /// Every outcome — preflight, rejection and upstream answer alike — leaves a
    /// single audit line with the correlation id, duration and status (PRD §9).
    pub async fn proxy(&self, call: ProxyCall) -> http::Response<ProxyBody> {
        let started = std::time::Instant::now();
        let tenant_id = call.tenant_id.clone();
        let method = call.method.clone();
        let path = call.path.clone();
        let correlation_id = call
            .headers
            .get(http::HeaderName::from_static("x-request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let response = self.proxy_once(call).await;
        tracing::info!(
            tenant = %tenant_id,
            method = %method,
            path = %path,
            status = response.status().as_u16(),
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            correlation_id = correlation_id.as_deref().unwrap_or("-"),
            "proxied request"
        );
        response
    }

    /// The proxy pipeline proper, called by [`Self::proxy`].
    async fn proxy_once(&self, call: ProxyCall) -> http::Response<ProxyBody> {
        // (1) Preflights never reach the upstream and need no tenant context.
        if cors::is_preflight(&call.method, &call.headers) {
            return cors::preflight_response(&call.headers);
        }

        // (2) The routing header is consumed here and never forwarded.
        let requested_host = match headers::requested_target_host(&call.headers) {
            Ok(host) => host,
            Err(e) => return self.error_response(&call, &[], e).await,
        };

        // (3) Alias resolution + route matching. Routes match the path that
        // follows the alias, not the full proxy path.
        let (alias, target) = match self.resolve_call(&call).await {
            Ok(resolved) => resolved,
            Err(e) => return self.error_response(&call, &[], e).await,
        };
        let bindings = target.plugins.clone();

        let mut ctx = RequestContext::new(
            call.tenant_id.clone(),
            alias.clone(),
            String::new(),
            Vec::new(),
            headers::strip_gateway_headers(&call.headers),
        );

        // (3b) Endpoint selection happens before the path is built so the
        // `Host` header can name the chosen endpoint.
        let endpoint = match self.select_endpoint(&target.upstream, requested_host.as_deref()) {
            Ok(e) => e,
            Err(e) => return self.error_response(&call, &bindings, e).await,
        };

        // (4) Route matching details: method/path/query contract.
        if let Err(e) = Self::apply_match_rules(&target, &call, &mut ctx) {
            return self.error_response(&call, &bindings, e).await;
        }

        // (5) Rate limiting.
        if let Some(limit) = target.rate_limit.as_ref()
            && let Err(rejection) = self.acquire_rate_limit(limit, &target, &call, &mut ctx)
        {
            let mut response = self.error_response(&call, &bindings, rejection.error).await;
            headers::insert_all(
                response.headers_mut(),
                &rate_limit::rate_limit_headers(limit, &rejection.decision),
            );
            return response;
        }

        // (6) CORS for actual cross-origin requests.
        if let Err(e) = Self::apply_cors(&target, &call, &mut ctx) {
            return self.error_response(&call, &bindings, e).await;
        }

        // (7) Plugin chain.
        if let Err(e) = self.run_plugins(&target, &mut ctx).await {
            return self.error_response(&call, &bindings, e).await;
        }

        // (8) Upstream call.
        if is_websocket_upgrade(&call.method, &call.headers) {
            return self.proxy_upgrade(call, target, endpoint, ctx).await;
        }
        match self.forward(&call, &target, &endpoint, &ctx).await {
            Ok(response) => response,
            Err(e) => self.error_response(&call, &bindings, e).await,
        }
    }

    /// Sends the buffered request upstream and normalizes the response.
    ///
    /// # Errors
    /// Returns the [`DomainError`] that aborted the outbound leg.
    async fn forward(
        &self,
        call: &ProxyCall,
        target: &ResolvedTarget,
        endpoint: &Endpoint,
        ctx: &RequestContext,
    ) -> Result<http::Response<ProxyBody>, DomainError> {
        let outbound =
            Self::build_outbound_request(target, endpoint, ctx, call.body.clone(), &call.method)?;
        let response = self.send(outbound).await?;
        Ok(self.finalize_response(response, target, ctx, call).await)
    }

    /// Resolves the proxy path to an alias and its configured target.
    ///
    /// # Errors
    /// Returns [`DomainError::RouteNotFound`] when the path does not name an
    /// alias or no route matches, and [`DomainError::NotFound`] for an unknown
    /// alias.
    async fn resolve_call(
        &self,
        call: &ProxyCall,
    ) -> Result<(String, ResolvedTarget), DomainError> {
        let alias = proxy_alias(&call.path);
        if alias.is_empty() {
            return Err(DomainError::RouteNotFound(
                "the proxy path must name an upstream alias".to_owned(),
            ));
        }
        let route_path = proxy_route_path(&call.path);
        let method = ProxyMethod::parse(call.method.as_str());
        let target = self
            .control_plane
            .resolve_proxy_target(&call.tenant_id, &alias, method, &route_path)
            .await?;
        Ok((alias, target))
    }

    // -----------------------------------------------------------------
    // Pipeline steps
    // -----------------------------------------------------------------

    /// Consumes one token from the effective bucket, when one applies.
    fn acquire_rate_limit(
        &self,
        limit: &crate::domain::model::RateLimitConfig,
        target: &ResolvedTarget,
        call: &ProxyCall,
        ctx: &mut RequestContext,
    ) -> Result<(), rate_limit::RateRejection> {
        let key = rate_limit::bucket_key(
            limit,
            &RateScopeContext {
                tenant_id: call.tenant_id.clone(),
                user_id: call.user_id.clone(),
                client_ip: call.client_ip.clone(),
                route_id: target.route.id.to_string(),
            },
        );
        let decision = self.limiter.try_acquire(&key, limit)?;
        ctx.attributes
            .set("oagw.rate_remaining", decision.remaining.to_string());
        Ok(())
    }

    /// Rejects actual cross-origin requests the upstream did not allow, and
    /// records the request's origin for the log.
    ///
    /// # Errors
    /// Returns [`DomainError::CorsOriginNotAllowed`] or
    /// [`DomainError::CorsMethodNotAllowed`].
    fn apply_cors(
        target: &ResolvedTarget,
        call: &ProxyCall,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let Some(cors_config) = target.cors.as_ref() else {
            return Ok(());
        };
        // A CORS block that is not enabled is not a policy: `validate_cors`
        // never checks the lists when it is stored, so honouring them here
        // would reject every cross-origin request against an empty allowlist.
        if !cors_config.enabled {
            return Ok(());
        }
        cors::validate_request(cors_config, &call.method, &call.headers)?;
        ctx.attributes
            .set("oagw.cors_origin", cors_origin(&call.headers));
        Ok(())
    }

    // -----------------------------------------------------------------
    // Endpoint selection
    // -----------------------------------------------------------------

    /// Picks the endpoint the request goes to.
    ///
    /// An explicit `X-OAGW-Target-Host` always wins; a pool without one
    /// round-robins unless the alias is the common suffix of the pool, in which
    /// case the header is required (ADR 0001, behaviour matrix).
    fn select_endpoint(
        &self,
        upstream: &crate::domain::model::Upstream,
        requested: Option<&str>,
    ) -> Result<Endpoint, DomainError> {
        let endpoints = &upstream.server.endpoints;
        // Plaintext is a configuration choice, not a scheme the model rejects:
        // an `http` endpoint is stored normally and refused only here, where a
        // connection would actually be made.
        if let Some(endpoint) = endpoints.first() {
            crate::domain::services::management::check_scheme_admission(
                endpoint.scheme,
                self.control_plane.allow_http_upstream(),
            )?;
        }
        if let Some(host) = requested {
            let wanted = alias::normalize_host(host);
            return endpoints
                .iter()
                .find(|e| alias::normalize_host(&e.host) == wanted)
                .cloned()
                .ok_or_else(|| DomainError::UnknownTargetHost(host.to_owned()));
        }
        match endpoints.len() {
            0 => Err(DomainError::LinkUnavailable(
                "upstream has no endpoints".to_owned(),
            )),
            1 => Ok(endpoints[0].clone()),
            _ if is_common_suffix_alias(upstream) => Err(DomainError::MissingTargetHost),
            _ => {
                let index = self.round_robin.fetch_add(1, Ordering::Relaxed) % endpoints.len();
                Ok(endpoints[index].clone())
            }
        }
    }

    // -----------------------------------------------------------------
    // Match rules
    // -----------------------------------------------------------------

    fn apply_match_rules(
        target: &ResolvedTarget,
        call: &ProxyCall,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let Some(http) = target.route.match_config.http.as_ref() else {
            return Err(DomainError::RouteNotFound(
                "route does not accept HTTP requests".to_owned(),
            ));
        };

        if !http
            .methods
            .iter()
            .any(|m| m.as_str().eq_ignore_ascii_case(call.method.as_str()))
        {
            // A method allowlist is a guard rule (DESIGN §"Guard Rules"), so the
            // rejection is a validation error even though the path did resolve.
            return Err(DomainError::Validation(format!(
                "method {} is not allowed by this route",
                call.method.as_str()
            )));
        }

        let remainder = target.path_remainder.trim_matches('/');
        if http.path_suffix_mode == PathSuffixMode::Disabled && !remainder.is_empty() {
            return Err(DomainError::Validation(
                "this route does not accept a path suffix".to_owned(),
            ));
        }
        ctx.path = if remainder.is_empty() {
            http.path.clone()
        } else {
            format!(
                "{}/{}",
                http.path.trim_end_matches('/'),
                remainder.trim_start_matches('/')
            )
        };

        // Query allowlist: an empty allowlist admits nothing, and a parameter
        // outside it rejects the request (DESIGN §Guard Rules).
        let parsed: Vec<(String, String)> = form_urlencoded::parse(call.query.as_bytes())
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        if let Some(key) = parsed
            .iter()
            .map(|(key, _)| key)
            .find(|key| !http.query_allowlist.iter().any(|a| a == *key))
        {
            return Err(DomainError::Validation(format!(
                "query parameter `{key}` is not allowed by this route"
            )));
        }
        ctx.query = parsed;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Plugin chain
    // -----------------------------------------------------------------

    async fn run_plugins(
        &self,
        target: &ResolvedTarget,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        // Auth first: the upstream's own `auth` block.
        if let Some(auth) = target.upstream.auth.as_ref()
            && let Some(plugin_type) = auth.plugin_type.as_deref()
            && let Some(plugin) = self.auth.resolve(plugin_type)?
        {
            let config = PluginConfig::from_binding(plugin_type, Some(&auth.config));
            plugin.authenticate(ctx, &config).await?;
        }

        // Guards run as a phase before transforms (DESIGN ADR 0002), whatever
        // order the two kinds were declared in; `target.plugins` is already
        // upstream-bound-first, so each pass keeps that precedence.
        for binding in &target.plugins {
            let config = PluginConfig::from_binding(binding.plugin_ref(), binding.config());
            if is_guard(binding.plugin_ref())
                && let Some(guard) = self.guards.resolve(binding.plugin_ref())?
            {
                guard.guard_request(ctx, &config).await?;
            }
        }
        for binding in &target.plugins {
            let config = PluginConfig::from_binding(binding.plugin_ref(), binding.config());
            if !is_guard(binding.plugin_ref())
                && let Some(transform) = self.transforms.resolve(binding.plugin_ref())?
            {
                transform.on_request(ctx, &config).await?;
            }
        }
        Ok(())
    }

    async fn run_response_plugins(
        &self,
        target: &ResolvedTarget,
        ctx: &mut ResponseContext<'_>,
    ) -> Result<(), DomainError> {
        // Response leg mirrors the request leg: guards as a phase, then
        // transforms.
        for binding in &target.plugins {
            let config = PluginConfig::from_binding(binding.plugin_ref(), binding.config());
            if is_guard(binding.plugin_ref())
                && let Some(guard) = self.guards.resolve(binding.plugin_ref())?
            {
                guard.guard_response(ctx, &config).await?;
            }
        }
        for binding in &target.plugins {
            let config = PluginConfig::from_binding(binding.plugin_ref(), binding.config());
            if !is_guard(binding.plugin_ref())
                && let Some(transform) = self.transforms.resolve(binding.plugin_ref())?
            {
                transform.on_response(ctx, &config).await?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Upstream call
    // -----------------------------------------------------------------

    fn build_outbound_headers(
        target: &ResolvedTarget,
        endpoint: &Endpoint,
        ctx: &RequestContext,
    ) -> HeaderMap {
        let mut out =
            headers::build_outbound_headers(&ctx.headers, &target.upstream.headers.request);
        // `Host` is replaced by the upstream host (DESIGN §3.2).
        if let Ok(host) = HeaderValue::from_str(&authority(endpoint)) {
            out.insert(http::header::HOST, host);
        }
        out
    }

    fn build_outbound_request(
        target: &ResolvedTarget,
        endpoint: &Endpoint,
        ctx: &RequestContext,
        body: Bytes,
        method: &Method,
    ) -> Result<http::Request<http_body_util::Full<Bytes>>, DomainError> {
        let uri = upstream_uri(endpoint, &ctx.path, &ctx.query_string())?;
        let mut builder = http::Request::builder()
            .method(method.clone())
            .version(http::Version::HTTP_11)
            .uri(uri);
        for (name, value) in &Self::build_outbound_headers(target, endpoint, ctx) {
            builder = builder.header(name.clone(), value.clone());
        }
        builder
            .body(http_body_util::Full::new(body))
            .map_err(|e| DomainError::Internal(e.to_string()))
    }

    async fn send(
        &self,
        request: http::Request<http_body_util::Full<Bytes>>,
    ) -> Result<http::Response<hyper::body::Incoming>, DomainError> {
        let future = self.client.request(request);
        match tokio::time::timeout(self.config.proxy_timeout(), future).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Err(if e.is_connect() {
                DomainError::DownstreamError(format!("could not connect to the upstream: {e}"))
            } else if timed_out(&e) {
                DomainError::ConnectionTimeout
            } else {
                DomainError::ProtocolError(e.to_string())
            }),
            Err(_) => Err(DomainError::RequestTimeout),
        }
    }

    /// Applies response rules, plugins and the error-source marker.
    async fn finalize_response(
        &self,
        response: http::Response<hyper::body::Incoming>,
        target: &ResolvedTarget,
        ctx: &RequestContext,
        call: &ProxyCall,
    ) -> http::Response<ProxyBody> {
        let (mut parts, body) = response.into_parts();
        headers::apply_response_headers(&mut parts.headers, &target.upstream.headers);

        let mut response_ctx = ResponseContext {
            status: parts.status,
            headers: parts.headers.clone(),
            request: ctx,
        };
        if let Err(e) = self.run_response_plugins(target, &mut response_ctx).await {
            return self.error_response(call, &target.plugins, e).await;
        }
        parts.status = response_ctx.status;
        parts.headers = response_ctx.headers;

        if let Some(origin) = ctx.attributes.get("oagw.cors_origin")
            && let Some(cors_config) = target.cors.as_ref()
        {
            cors::apply_origin_headers(&mut parts.headers, cors_config, origin);
        }
        parts.headers.insert(
            header_name(gts::HEADER_ERROR_SOURCE),
            header_value(gts::ERROR_SOURCE_UPSTREAM),
        );
        http::Response::from_parts(parts, ProxyBody::new(body))
    }

    // -----------------------------------------------------------------
    // WebSocket
    // -----------------------------------------------------------------

    /// Proxies a WebSocket upgrade, bridging the two connections afterwards.
    async fn proxy_upgrade(
        &self,
        call: ProxyCall,
        target: ResolvedTarget,
        endpoint: Endpoint,
        ctx: RequestContext,
    ) -> http::Response<ProxyBody> {
        // The upgrade handshake must be forwarded verbatim, hop-by-hop headers
        // and all, or the upstream cannot complete it. The gateway's own
        // routing/marking headers are the one exception: they direct the proxy
        // and are consumed here just as they are on the plain-HTTP path.
        let mut outbound_headers = HeaderMap::new();
        for (name, value) in &call.headers {
            if name == gts::HEADER_TARGET_HOST || name == gts::HEADER_ERROR_SOURCE {
                continue;
            }
            outbound_headers.append(name.clone(), value.clone());
        }
        if let Ok(host) = HeaderValue::from_str(&authority(&endpoint)) {
            outbound_headers.insert(http::header::HOST, host);
        }

        let uri = match upstream_uri(&endpoint, &ctx.path, &ctx.query_string()) {
            Ok(u) => u,
            Err(e) => return self.error_response(&call, &target.plugins, e).await,
        };
        let mut request = match http::Request::builder()
            .method(call.method.clone())
            .version(http::Version::HTTP_11)
            .uri(uri)
            .body(http_body_util::Full::new(Bytes::new()))
        {
            Ok(r) => r,
            Err(e) => {
                return self
                    .error_response(&call, &target.plugins, DomainError::Internal(e.to_string()))
                    .await;
            }
        };
        *request.headers_mut() = outbound_headers;

        let client_upgrade = call.upgrade.clone();
        let mut response = match self.send(request).await {
            Ok(r) => r,
            Err(e) => return self.error_response(&call, &target.plugins, e).await,
        };

        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            // The upstream refused the upgrade; pass its answer through.
            return self.finalize_response(response, &target, &ctx, &call).await;
        }

        let Some(pending) = client_upgrade else {
            return self
                .error_response(
                    &call,
                    &target.plugins,
                    DomainError::ProtocolError(
                        "the client connection did not request an upgrade".to_owned(),
                    ),
                )
                .await;
        };

        // `on` reads the upgrade out of the response extensions, so it must run
        // before the response is dismantled.
        let upstream_io = match hyper::upgrade::on(&mut response).await {
            Ok(io) => io,
            Err(e) => {
                return self
                    .error_response(
                        &call,
                        &target.plugins,
                        DomainError::StreamAborted(format!("upstream upgrade failed: {e}")),
                    )
                    .await;
            }
        };
        let (mut parts, body) = response.into_parts();
        drop(body);
        parts.headers.insert(
            header_name(gts::HEADER_ERROR_SOURCE),
            header_value(gts::ERROR_SOURCE_UPSTREAM),
        );

        let tenant = call.tenant_id.clone();
        let alias = ctx.alias.clone();
        tokio::spawn(async move {
            let pending = pending;
            let client_io = match pending.await {
                Ok(io) => io,
                Err(e) => {
                    tracing::warn!(tenant = %tenant, alias = %alias, "client upgrade failed: {e}");
                    return;
                }
            };
            // `Upgraded` speaks hyper's own `Read`/`Write`; `TokioIo` adapts it
            // to the tokio traits `copy_bidirectional` needs.
            let mut a = TokioIo::new(client_io);
            let mut b = TokioIo::new(upstream_io);
            if let Err(e) = tokio::io::copy_bidirectional(&mut a, &mut b).await {
                tracing::debug!(tenant = %tenant, alias = %alias, "websocket relay ended: {e}");
            }
        });

        http::Response::from_parts(parts, ProxyBody::empty())
    }

    // -----------------------------------------------------------------
    // Errors
    // -----------------------------------------------------------------

    /// Builds a gateway error response, running the plugins' error phase.
    async fn error_response(
        &self,
        call: &ProxyCall,
        bindings: &[PluginBinding],
        error: DomainError,
    ) -> http::Response<ProxyBody> {
        let ctx = RequestContext::new(
            call.tenant_id.clone(),
            proxy_alias(&call.path),
            call.path.clone(),
            Vec::new(),
            headers::strip_gateway_headers(&call.headers),
        );
        let mut error_ctx = ErrorContext {
            status: error.status(),
            headers: HeaderMap::new(),
            request: &ctx,
            error: &error,
        };
        for binding in bindings {
            let config = PluginConfig::from_binding(binding.plugin_ref(), binding.config());
            if let Ok(Some(transform)) = self.transforms.resolve(binding.plugin_ref())
                && transform.on_error(&mut error_ctx, &config).await.is_err()
            {
                // A failing error-phase plugin falls back to the default body.
                error_ctx.headers.clear();
                break;
            }
        }
        let mut response = crate::api::rest::error::problem_response(&error, Some(&ctx.path));
        for (name, value) in &error_ctx.headers {
            response.headers_mut().append(name.clone(), value.clone());
        }
        response
    }
}

/// Whether the request asks for a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::GET
        && headers
            .get(http::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.to_ascii_lowercase()
                    .split(',')
                    .any(|p| p.trim() == "upgrade")
            })
        && headers
            .get(http::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Whether an upstream's alias is the common registrable suffix of its pool.
fn is_common_suffix_alias(upstream: &crate::domain::model::Upstream) -> bool {
    matches!(
        alias::compute_derived_alias(&upstream.server.endpoints),
        Ok(AliasDerivation::Derived(derived)) if derived == upstream.alias
    )
}

/// `host[:port]` for the `Host` header and the URL authority.
#[must_use]
fn authority(endpoint: &Endpoint) -> String {
    let host = alias::normalize_host(&endpoint.host);
    if endpoint.has_standard_port() {
        host
    } else {
        format!("{host}:{}", endpoint.effective_port())
    }
}

/// Builds the upstream URL from the endpoint, the route path and the query.
fn upstream_uri(endpoint: &Endpoint, path: &str, query: &str) -> Result<Uri, DomainError> {
    let scheme = match endpoint.scheme {
        EndpointScheme::Http => Scheme::HTTP,
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
            Scheme::HTTPS
        }
    };
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    Uri::builder()
        .scheme(scheme)
        .authority(authority(endpoint))
        .path_and_query(if query.is_empty() {
            path
        } else {
            format!("{path}?{query}")
        })
        .build()
        .map_err(|e| DomainError::Validation(format!("upstream URI is not buildable: {e}")))
}

/// The alias addressed by a proxy path: the segment right after `/proxy/`.
///
/// The caller supplies everything after `/proxy/`, so the first segment is the
/// alias and the rest is the path suffix.
#[must_use]
pub fn proxy_alias(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    let (alias, _) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    alias::normalize_host(alias)
}

/// The part of the proxy path the route table matches against: everything after
/// the alias segment, with a leading `/`.
#[must_use]
pub fn proxy_route_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    match trimmed.split_once('/') {
        Some((_, rest)) => format!("/{rest}"),
        None => "/".to_owned(),
    }
}

/// The `Origin` header value of a request, when cross-origin.
#[must_use]
fn cors_origin(headers: &HeaderMap) -> String {
    headers
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Builds a `HeaderName`, panicking only on a static, known-valid name.
#[must_use]
#[allow(clippy::expect_used)] // callers pass literal header names
fn header_name(name: &str) -> http::HeaderName {
    http::HeaderName::from_bytes(name.as_bytes()).expect("static header name")
}

/// Builds a `HeaderValue` from a static string.
#[must_use]
fn header_value(value: &'static str) -> HeaderValue {
    HeaderValue::from_static(value)
}

/// Whether a client error is a timeout of the underlying socket.
fn timed_out(error: &hyper_util::client::legacy::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            );
        }
        source = err.source();
    }
    false
}

/// Whether a plugin reference addresses the guard registry.
///
/// Built-ins carry their kind in the GTS type; a custom plugin is a bare UUID
/// and is tried in whichever registry holds it.
fn is_guard(plugin_ref: &str) -> bool {
    plugin_ref.starts_with(crate::domain::gts_helpers::GUARD_PLUGIN_TYPE)
}
