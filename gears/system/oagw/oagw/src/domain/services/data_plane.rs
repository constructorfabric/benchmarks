//! Data plane service — proxy request orchestration (DESIGN §3.3/§3.5).
//!
//! The service drives one proxy exchange end to end:
//!
//! 1. resolve the effective upstream + route via the control plane
//! 2. select the upstream endpoint (`X-OAGW-Target-Host` matrix)
//! 3. enforce per-scope token-bucket rate limits (ADR-0003)
//! 4. validate the actual CORS request origin/method (ADR-0004)
//! 5. run the plugin chain `Auth → Guard(request) → Transform(request)`
//! 6. forward to the upstream (streaming) and run the response chain
//!    `Transform(response) → Guard(response)`, apply header + CORS rules
//! 7. tag every response with `X-OAGW-Error-Source` (ADR-0007)

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use http::header::{CONTENT_LENGTH, HOST, ORIGIN};
use http::{HeaderMap, HeaderValue, Method};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::dto::{
    EffectiveUpstream, Endpoint, EndpointScheme, HeaderTransform, PassthroughMode, RateScope,
    RateStrategy,
};
use crate::domain::error::{DomainError, ProblemContext};
use crate::domain::gts_helpers;
use crate::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginConfig, PluginError,
    RequestContext, ResponseContext, TransformPlugin,
};
use crate::domain::services::management::{ControlPlaneService, ResolvedTarget};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::cors;
use crate::infra::proxy::forwarder::{ForwardError, Forwarder};
use crate::infra::proxy::rate_limiter::{RateLimitHeaders, RateLimiter};

/// `X-OAGW-Error-Source` header (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Gateway-originated error/value.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Upstream passthrough value.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";
/// Upper bound (seconds) a `queue`-strategy rate limit waits for capacity
/// before falling back to a rejection.
const MAX_QUEUE_WAIT_SECS: u64 = 5;

/// Where an error response originated (ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway produced the response.
    Gateway,
    /// The upstream produced the response (or it is a passthrough success).
    Upstream,
}

/// A fully-resolved proxy exchange result — an already-streaming HTTP
/// response plus its error-source tag.
#[derive(Debug)]
pub struct ProxyResponse {
    /// Whether the payload came from the upstream or was gateway-generated.
    pub source: ErrorSource,
    /// The response to stream back to the client.
    pub response: http::Response<axum::body::Body>,
}

/// Inbound proxy request assembled by the REST handler.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Caller security context (drives tenant scoping + secret resolution).
    pub security_context: toolkit_security::SecurityContext,
    /// Tenant that owns the proxy call.
    pub tenant_id: Uuid,
    /// Upstream alias from the URL.
    pub alias: String,
    /// Full request path after the alias (starts with `/`; includes any
    /// suffix).
    pub path: String,
    /// HTTP method.
    pub method: Method,
    /// Inbound request headers.
    pub headers: HeaderMap,
    /// Raw query string (without leading `?`).
    pub query: String,
    /// Buffered request body (size-capped by the handler).
    pub body: bytes::Bytes,
    /// Client IP (used for `scope: ip` rate limiting).
    pub client_ip: Option<IpAddr>,
}

/// Prepared WebSocket target — the upstream URL and the final outbound
/// headers (after the plugin chain) used for the client handshake.
#[derive(Debug, Clone)]
pub struct WsTarget {
    /// `ws(s)://host:port/path?query`.
    pub url: String,
    /// Outbound handshake headers (routing/hop-by-hop already stripped).
    pub headers: HeaderMap,
}

/// Proxy orchestrator.
pub struct DataPlaneService {
    control: Arc<ControlPlaneService>,
    auth_registry: Arc<AuthPluginRegistry>,
    guard_registry: Arc<GuardPluginRegistry>,
    transform_registry: Arc<TransformPluginRegistry>,
    forwarder: Forwarder,
    rate_limiter: RateLimiter,
    round_robin: AtomicUsize,
    config: OagwConfig,
}

/// Headers consumed by the gateway during routing / per HTTP semantics.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "sec-websocket-version",
    "sec-websocket-key",
    "sec-websocket-extensions",
];
/// Routing headers that are read by OAGW and never forwarded.
const ROUTING_HEADERS: &[&str] = &["x-oagw-target-host"];

/// Resolved plugin bindings, split by family with their per-binding config.
struct ResolvedChain {
    auth: Option<(PluginConfig, Arc<dyn AuthPlugin>)>,
    guards: Vec<(PluginConfig, Arc<dyn GuardPlugin>)>,
    transforms: Vec<(PluginConfig, Arc<dyn TransformPlugin>)>,
}

impl DataPlaneService {
    /// Build the service with the given control plane, registries and gear
    /// config (the forwarder is derived from the config).
    #[must_use]
    pub fn new(
        control: Arc<ControlPlaneService>,
        auth_registry: Arc<AuthPluginRegistry>,
        guard_registry: Arc<GuardPluginRegistry>,
        transform_registry: Arc<TransformPluginRegistry>,
        config: OagwConfig,
    ) -> Self {
        let forwarder = Forwarder::new(config.proxy_timeout(), config.max_body_bytes);
        Self {
            control,
            auth_registry,
            guard_registry,
            transform_registry,
            forwarder,
            rate_limiter: RateLimiter::new(),
            round_robin: AtomicUsize::new(0),
            config,
        }
    }

    /// Execute a full HTTP proxy exchange.
    ///
    /// # Errors
    /// Returns a gateway `DomainError` for any pre-forward failure; upstream
    /// responses (including upstream errors) are returned as `ProxyResponse`
    /// with `source: Upstream`.
    pub async fn execute(
        &self,
        ctx: &toolkit_security::SecurityContext,
        req: &ProxyRequest,
    ) -> Result<ProxyResponse, DomainError> {
        let target = self
            .control
            .resolve_proxy_target(
                ctx,
                req.tenant_id,
                &req.alias,
                req.method.as_str(),
                &req.path,
            )
            .await?;
        let problem_ctx = self.problem_context(&target, &req.path);

        self.validate_body(&req.headers, req.body.len())?;
        self.validate_query_allowlist(&target, req)?;

        let endpoint = self.select_endpoint(&target.upstream, target_host(&req.headers))?;
        let rate_headers = self.check_rate_limit(ctx, &target, req).await?;

        // CORS actual-request validation (ADR-0004) — after resolution,
        // before forwarding.
        self.check_cors_actual(&target, req)?;

        let chain = self.resolve_chain(&target)?;
        let (out_path, out_query, out_headers, out_body, out_method) =
            self.build_outbound(&target, req, &endpoint, &chain).await?;

        let Some(url) = self.endpoint_http_url(&endpoint, &out_path, &out_query) else {
            return Err(DomainError::ProtocolError {
                detail: format!(
                    "endpoint scheme {:?} is not support for HTTP forwarding",
                    endpoint.scheme
                ),
                context: Some(problem_ctx),
            });
        };

        let mut request = http::Request::builder()
            .method(out_method)
            .uri(&url)
            .body(axum::body::Body::from(out_body))
            .map_err(|e| {
                DomainError::validation(format!("failed to build upstream request: {e}"))
            })?;
        *request.headers_mut() = out_headers;

        let upstream_resp = self
            .forwarder
            .send(request)
            .await
            .map_err(|e| self.map_forward_error(e, &target, &req.path))?;

        // Response plugin chain + response transforms + CORS headers.
        // Status/headers are taken by value (never a `&Response<Body>` held
        // across an `.await` — Body is not `Sync`, which would make the
        // future non-`Send`); the chain returns the (possibly mutated)
        // status and headers.
        let (mut resp_headers, resp_status) = match self
            .run_response_chain(
                &target,
                &chain,
                upstream_resp.status(),
                upstream_resp.headers().clone(),
            )
            .await
        {
            Ok((status, headers)) => (headers, status),
            Err(err) => return Err(self.apply_transform_error(err, &target, &chain).await),
        };

        self.apply_response_transforms(&target, &mut resp_headers);
        self.apply_cors_response(&target, req, &mut resp_headers);
        if let Some(rate) = &rate_headers {
            self.apply_rate_headers(&target, &mut resp_headers, rate);
        }
        strip_hop_by_hop(&mut resp_headers);
        resp_headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );

        let mut response: http::Response<axum::body::Body> = upstream_resp;
        *response.status_mut() = resp_status;
        *response.headers_mut() = resp_headers;

        Ok(ProxyResponse {
            source: ErrorSource::Upstream,
            response,
        })
    }

    /// Prepare a WebSocket proxy exchange: resolve, route, rate-limit, run the
    /// request-side plugin chain and produce the upstream dial target.
    ///
    /// # Errors
    /// Any gateway error that should short-circuit the upgrade.
    pub async fn prepare_websocket(
        &self,
        ctx: &toolkit_security::SecurityContext,
        req: &ProxyRequest,
    ) -> Result<WsTarget, DomainError> {
        let target = self
            .control
            .resolve_proxy_target(
                ctx,
                req.tenant_id,
                &req.alias,
                req.method.as_str(),
                &req.path,
            )
            .await?;

        let endpoint = self.select_endpoint(&target.upstream, target_host(&req.headers))?;
        let _ = self.check_rate_limit(ctx, &target, req).await?;
        self.check_cors_actual(&target, req)?;

        let chain = self.resolve_chain(&target)?;
        let (out_path, out_query, out_headers, ..) =
            self.build_outbound(&target, req, &endpoint, &chain).await?;

        let url = self
            .endpoint_ws_url(&endpoint, &out_path, &out_query)
            .ok_or_else(|| DomainError::ProtocolError {
                detail: format!(
                    "endpoint scheme {:?} is not supported for WebSocket bridging",
                    endpoint.scheme
                ),
                context: Option::default(),
            })?;

        Ok(WsTarget {
            url,
            headers: out_headers,
        })
    }

    // ------------------------------------------------------------------
    // Resolution helpers
    // ------------------------------------------------------------------

    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn problem_context(&self, target: &ResolvedTarget, path: &str) -> ProblemContext {
        ProblemContext {
            upstream_id: Some(gts_helpers::upstream_resource_id(target.upstream.id)),
            alias: Some(target.upstream.alias.clone()),
            host: Some(
                target
                    .upstream
                    .server
                    .endpoints
                    .first()
                    .map(Endpoint::normalized_host)
                    .unwrap_or_default(),
            ),
            path: Some(path.to_owned()),
            ..ProblemContext::new()
        }
    }

    /// Validate the request body constraints (DESIGN body validation rules).
    #[allow(
        clippy::unused_self, // kept as a method for call-site uniformity
        clippy::result_large_err // DomainError is a broad domain error enum; boxing would ripple through every caller
    )]
    fn validate_body(&self, headers: &HeaderMap, body_len: usize) -> Result<(), DomainError> {
        if let Some(te) = headers.get("transfer-encoding") {
            let value = te.to_str().unwrap_or_default().to_ascii_lowercase();
            // Only `chunked` is accepted — any other coding (e.g. `gzip,
            // chunked`) is a smuggling risk and is rejected.
            let codings: Vec<&str> = value.split(',').map(str::trim).collect();
            if codings.is_empty() || codings.iter().any(|c| !c.eq_ignore_ascii_case("chunked")) {
                return Err(DomainError::validation(format!(
                    "unsupported transfer-encoding '{value}'; only 'chunked' is accepted"
                )));
            }
            if headers.contains_key(CONTENT_LENGTH) {
                return Err(DomainError::validation(
                    "both content-length and transfer-encoding present (smuggling risk)",
                ));
            }
        }
        if let Some(cl) = headers.get(CONTENT_LENGTH) {
            let raw = cl.to_str().unwrap_or_default();
            match raw.parse::<usize>() {
                Ok(len) if len == body_len => {}
                Ok(len) => {
                    return Err(DomainError::validation(format!(
                        "content-length {len} does not match actual body size {body_len}"
                    )));
                }
                Err(_) => {
                    return Err(DomainError::validation(format!(
                        "invalid content-length header '{raw}'"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Enforce the route's `query_allowlist`: any present query parameter not
    /// on the allowlist rejects the exchange before forwarding.
    #[allow(
        clippy::unused_self, // kept as a method for call-site uniformity
        clippy::result_large_err // DomainError is a broad domain error enum; boxing would ripple through every caller
    )]
    fn validate_query_allowlist(
        &self,
        target: &ResolvedTarget,
        req: &ProxyRequest,
    ) -> Result<(), DomainError> {
        let Some(http) = target.route.r#match.as_http() else {
            return Ok(());
        };
        if http.query_allowlist.is_empty() {
            return Ok(());
        }
        if let Some(param) = unknown_query_params(&req.query, &http.query_allowlist) {
            return Err(DomainError::validation(format!(
                "query parameter '{param}' is not allowed for this route"
            )));
        }
        Ok(())
    }

    /// `X-OAGW-Target-Host` endpoint selection matrix (DESIGN §3.5).
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    fn select_endpoint(
        &self,
        upstream: &EffectiveUpstream,
        target_host: Option<&str>,
    ) -> Result<Endpoint, DomainError> {
        let eps = &upstream.server.endpoints;
        let valid_hosts: Vec<String> = eps.iter().map(Endpoint::normalized_host).collect();
        let context = || ProblemContext {
            upstream_id: Some(gts_helpers::upstream_resource_id(upstream.id)),
            alias: Some(upstream.alias.clone()),
            valid_hosts: Some(valid_hosts.clone()),
            ..ProblemContext::new()
        };

        if let Some(raw) = target_host {
            if !is_valid_target_host_value(raw) {
                return Err(DomainError::InvalidTargetHost {
                    detail: "X-OAGW-Target-Host must be a valid hostname or IP (no port, path or special characters)".to_owned(),
                    value: raw.to_owned(),
                    context: Some(context()),
                });
            }
            let normalized = raw.trim_end_matches('.').to_ascii_lowercase();
            let Some(ep) = eps.iter().find(|e| e.normalized_host() == normalized) else {
                return Err(DomainError::UnknownTargetHost {
                    detail: format!(
                        "X-OAGW-Target-Host '{raw}' does not match any configured endpoint"
                    ),
                    value: raw.to_owned(),
                    valid_hosts,
                    context: Some(ProblemContext {
                        upstream_id: Some(gts_helpers::upstream_resource_id(upstream.id)),
                        alias: Some(upstream.alias.clone()),
                        ..ProblemContext::new()
                    }),
                });
            };
            return Ok(ep.clone());
        }

        match eps.len() {
            1 => Ok(eps[0].clone()),
            _ if upstream.alias_is_common_suffix => Err(DomainError::MissingTargetHost {
                detail: "X-OAGW-Target-Host header required for a multi-endpoint upstream with a common-suffix alias"
                    .to_owned(),
                valid_hosts,
                context: Some(ProblemContext {
                    upstream_id: Some(gts_helpers::upstream_resource_id(upstream.id)),
                    alias: Some(upstream.alias.clone()),
                    ..ProblemContext::new()
                }),
            }),
            _ => {
                // Multi-endpoint pool with an explicit alias → round-robin.
                let idx = self.round_robin.fetch_add(1, Relaxed) % eps.len();
                Ok(eps[idx].clone())
            }
        }
    }

    /// Enforce the effective rate limit for this exchange (ADR-0003),
    /// honouring the configured strategy (`reject` / `queue` / `degrade`).
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    async fn check_rate_limit(
        &self,
        ctx: &toolkit_security::SecurityContext,
        target: &ResolvedTarget,
        req: &ProxyRequest,
    ) -> Result<Option<RateLimitHeaders>, DomainError> {
        let Some(cfg) = &target.route.rate_limit else {
            return Ok(None);
        };
        let key = self.rate_key(ctx, target, req, cfg.scope);

        let mut headers = self.rate_limiter.check(&key, cfg);
        if let Some(retry_after) = headers.retry_after_secs {
            match cfg.strategy {
                RateStrategy::Reject => {
                    return Err(rate_exceeded(target, req, &headers, retry_after));
                }
                RateStrategy::Queue => {
                    // Bounded wait for capacity (never blocks the caller for
                    // longer than [`MAX_QUEUE_WAIT_SECS`]), then re-check.
                    let wait = Duration::from_secs(retry_after.min(MAX_QUEUE_WAIT_SECS));
                    tokio::time::sleep(wait).await;
                    headers = self.rate_limiter.check(&key, cfg);
                    if let Some(retry_after) = headers.retry_after_secs {
                        return Err(rate_exceeded(target, req, &headers, retry_after));
                    }
                }
                // Degrade: let the request through without capacity.
                RateStrategy::Degrade => {}
            }
        }
        // X-RateLimit-* headers are only emitted when the config asks for them.
        if !cfg.response_headers {
            return Ok(None);
        }
        Ok(Some(headers))
    }

    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn rate_key(
        &self,
        ctx: &toolkit_security::SecurityContext,
        target: &ResolvedTarget,
        req: &ProxyRequest,
        scope: RateScope,
    ) -> String {
        match scope {
            RateScope::Global => format!("{}|g", target.upstream.id),
            RateScope::Tenant => format!("{}|t|{}", target.upstream.id, req.tenant_id),
            RateScope::User => format!("{}|u|{}", target.upstream.id, ctx.subject_id()),
            RateScope::Ip => format!(
                "{}|i|{}",
                target.upstream.id,
                req.client_ip
                    .map_or_else(|| "unknown".to_owned(), |ip| ip.to_string())
            ),
            RateScope::Route => format!("{}|r|{}", target.upstream.id, target.route.id),
        }
    }

    /// CORS validation for actual requests (ADR-0004) — only when an origin is
    /// present and CORS is enabled.
    #[allow(
        clippy::unused_self, // kept as a method for call-site uniformity
        clippy::result_large_err // DomainError is a broad domain error enum; boxing would ripple through every caller
    )]
    fn check_cors_actual(
        &self,
        target: &ResolvedTarget,
        req: &ProxyRequest,
    ) -> Result<(), DomainError> {
        let Some(cors_cfg) = cors::effective_cors(&target.upstream, &target.route).cloned() else {
            return Ok(());
        };
        if !cors_cfg.enabled {
            return Ok(());
        }
        let Some(origin) = req.headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
            return Ok(());
        };
        cors::check_actual_origin(&cors_cfg, origin, &req.path)?;
        cors::check_actual_method(&cors_cfg, req.method.as_str())?;
        Ok(())
    }

    /// Apply the CORS response headers for an allowed actual request
    /// (ADR-0004) — mirrors `cors::apply_actual_headers`.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn apply_cors_response(
        &self,
        target: &ResolvedTarget,
        req: &ProxyRequest,
        headers: &mut HeaderMap,
    ) {
        let Some(cors_cfg) = cors::effective_cors(&target.upstream, &target.route) else {
            return;
        };
        if !cors_cfg.enabled {
            return;
        }
        let Some(origin) = req.headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
            return;
        };
        if let Ok(value) = HeaderValue::from_str(origin) {
            headers.insert("access-control-allow-origin", value);
        }
        if cors_cfg.allow_credentials {
            headers.insert(
                "access-control-allow-credentials",
                HeaderValue::from_static("true"),
            );
        }
        if !cors_cfg.expose_headers.is_empty()
            && let Ok(value) = HeaderValue::from_str(&cors_cfg.expose_headers.join(", "))
        {
            headers.insert("access-control-expose-headers", value);
        }
        headers.append(http::header::VARY, HeaderValue::from_static("Origin"));
    }

    /// Resolve every bound plugin into an executable chain (builtins only;
    /// unresolvable references → `PluginNotFound`).
    #[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
    fn resolve_chain(&self, target: &ResolvedTarget) -> Result<ResolvedChain, DomainError> {
        let auth = match &target.upstream.auth {
            Some(auth) => match auth.plugin_type.as_deref() {
                Some(plugin_type) => Some((
                    auth.config.clone(),
                    self.auth_registry
                        .resolve(plugin_type)
                        .ok_or_else(|| plugin_missing(plugin_type, target))?,
                )),
                None => None,
            },
            None => None,
        };

        let mut guards: Vec<(PluginConfig, Arc<dyn GuardPlugin>)> = Vec::new();
        let mut transforms: Vec<(PluginConfig, Arc<dyn TransformPlugin>)> = Vec::new();
        for binding in &target.route.plugins.items {
            if let Some(guard) = self.guard_registry.resolve(&binding.plugin_ref) {
                guards.push((binding.config.clone(), guard));
            } else if let Some(transform) = self.transform_registry.resolve(&binding.plugin_ref) {
                transforms.push((binding.config.clone(), transform));
            } else {
                return Err(plugin_missing(&binding.plugin_ref, target));
            }
        }

        Ok(ResolvedChain {
            auth,
            guards,
            transforms,
        })
    }

    /// Build the final outbound path / query / headers / body / method for
    /// the upstream, running the request-side plugin chain
    /// (`Auth → Guard → Transform`).
    async fn build_outbound(
        &self,
        target: &ResolvedTarget,
        req: &ProxyRequest,
        endpoint: &Endpoint,
        chain: &ResolvedChain,
    ) -> Result<(String, String, HeaderMap, bytes::Bytes, http::Method), DomainError> {
        let headers = make_outbound_headers(&req.headers, &target.upstream.headers.request);
        let mut rctx = RequestContext {
            method: req.method.clone(),
            path: req.path.clone(),
            query: req.query.clone(),
            headers,
            body: req.body.clone(),
            security_context: req.security_context.clone(),
            upstream_id: target.upstream.id,
            upstream_alias: target.upstream.alias.clone(),
            tenant_id: target.upstream.tenant_id,
            config: PluginConfig::new(),
            endpoint: Some(endpoint.clone()),
        };

        // Auth first.
        if let Some((config, plugin)) = &chain.auth {
            rctx.config = config.clone();
            match plugin.authenticate(&mut rctx).await {
                Ok(()) => {}
                Err(err) => return Err(map_plugin_error(err)),
            }
        }
        // Guards (request) — all before any transform.
        for (config, plugin) in &chain.guards {
            rctx.config = config.clone();
            match plugin.guard_request(&rctx).await {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject {
                    status,
                    code,
                    detail,
                }) => {
                    return Err(map_plugin_error(PluginError::Reject {
                        detail,
                        code,
                        status,
                    }));
                }
                Err(err) => return Err(map_plugin_error(err)),
            }
        }
        // Transforms (request).
        for (config, plugin) in &chain.transforms {
            rctx.config = config.clone();
            if let Err(err) = plugin.transform_request(&mut rctx).await {
                return Err(map_plugin_error(err));
            }
        }

        // Rewrite the transport headers for the upstream leg.
        replace_host_header(&mut rctx.headers, endpoint);
        rctx.headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&rctx.body.len().to_string()).map_err(|e| {
                DomainError::Internal {
                    detail: format!("invalid content-length: {e}"),
                    source: None,
                }
            })?,
        );

        Ok((rctx.path, rctx.query, rctx.headers, rctx.body, rctx.method))
    }

    /// Run the response-side chain: `Transform(response) → Guard(response)`.
    ///
    /// Takes the response status and headers by value (rather than a
    /// `&Response<Body>`) so the future stays `Send` across the plugin
    /// awaits, and returns the (possibly transformed) status and headers for
    /// the caller to apply to the forwarded response.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    async fn run_response_chain(
        &self,
        target: &ResolvedTarget,
        chain: &ResolvedChain,
        upstream_status: http::StatusCode,
        upstream_headers: http::HeaderMap,
    ) -> Result<(http::StatusCode, http::HeaderMap), DomainError> {
        let mut rctx = ResponseContext {
            status: upstream_status,
            headers: upstream_headers,
            upstream_id: target.upstream.id,
            upstream_alias: target.upstream.alias.clone(),
            config: PluginConfig::new(),
        };
        for (config, plugin) in &chain.transforms {
            rctx.config = config.clone();
            if let Err(err) = plugin.transform_response(&mut rctx).await {
                return Err(map_plugin_error(err));
            }
        }
        for (config, plugin) in &chain.guards {
            rctx.config = config.clone();
            match plugin.guard_response(&rctx).await {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject {
                    status,
                    code,
                    detail,
                }) => {
                    return Err(map_plugin_error(PluginError::Reject {
                        detail,
                        code,
                        status,
                    }));
                }
                Err(err) => return Err(map_plugin_error(err)),
            }
        }
        Ok((rctx.status, rctx.headers))
    }

    /// Run `TransformPlugin::transform_error` over the failure and fold the
    /// mutations back into the gateway error.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    async fn apply_transform_error(
        &self,
        err: DomainError,
        target: &ResolvedTarget,
        chain: &ResolvedChain,
    ) -> DomainError {
        let info = err.problem_info();
        let mut ectx = ErrorContext {
            upstream_id: Some(target.upstream.id),
            alias: Some(target.upstream.alias.clone()),
            status: info.status,
            code: info.detail.clone(),
            detail: info.detail,
            config: PluginConfig::new(),
        };
        let original_detail = ectx.detail.clone();
        let mut poisoned: Option<PluginError> = None;
        for (config, plugin) in &chain.transforms {
            ectx.config = config.clone();
            if let Err(plugin_err) = plugin.transform_error(&mut ectx).await {
                poisoned = Some(plugin_err);
                break;
            }
        }
        if let Some(plugin_err) = poisoned {
            return map_plugin_error(plugin_err);
        }
        if ectx.detail != original_detail || ectx.status != info.status {
            let mut mapped = gateway_error(ectx.status, ectx.detail, ProblemContext::new());
            match &mut mapped {
                DomainError::Validation { context, .. }
                | DomainError::AuthFailed { context, .. }
                | DomainError::CorsOriginNotAllowed { context, .. }
                | DomainError::CorsMethodNotAllowed { context, .. }
                | DomainError::PayloadTooLarge { context, .. }
                | DomainError::RateLimitExceeded { context, .. }
                | DomainError::SecretNotFound { context, .. }
                | DomainError::ProtocolError { context, .. }
                | DomainError::DownstreamError { context, .. }
                | DomainError::StreamAborted { context, .. }
                | DomainError::LinkUnavailable { context, .. }
                | DomainError::CircuitBreakerOpen { context, .. }
                | DomainError::PluginNotFound { context, .. }
                | DomainError::TimeoutConnection { context, .. }
                | DomainError::TimeoutRequest { context, .. } => {
                    *context = Some(ProblemContext {
                        upstream_id: Some(gts_helpers::upstream_resource_id(target.upstream.id)),
                        alias: Some(target.upstream.alias.clone()),
                        ..ProblemContext::new()
                    });
                }
                _ => {}
            }
            return mapped;
        }
        err
    }

    /// Apply the configured upstream response header transforms.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn apply_response_transforms(&self, target: &ResolvedTarget, headers: &mut HeaderMap) {
        apply_side_transform(headers, &target.upstream.headers.response);
    }

    /// Apply `X-RateLimit-*` headers to an allowed response.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn apply_rate_headers(
        &self,
        _target: &ResolvedTarget,
        headers: &mut HeaderMap,
        rate: &RateLimitHeaders,
    ) {
        headers.insert(
            "x-ratelimit-limit",
            HeaderValue::from_str(&rate.limit.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        );
        headers.insert(
            "x-ratelimit-remaining",
            HeaderValue::from_str(&rate.remaining.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        );
        headers.insert(
            "x-ratelimit-reset",
            HeaderValue::from_str(&rate.reset_epoch.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        );
    }

    /// Build the upstream HTTP(S) URL for the selected endpoint.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn endpoint_http_url(&self, endpoint: &Endpoint, path: &str, query: &str) -> Option<String> {
        let scheme = match endpoint.scheme {
            EndpointScheme::Http => "http",
            EndpointScheme::Https => "https",
            _ => return None,
        };
        let authority = authority(endpoint);
        let mut url = format!("{scheme}://{authority}{path}");
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        Some(url)
    }

    /// Build the upstream `ws(s)://` URL for the selected endpoint.
    #[allow(clippy::unused_self)] // kept as a method for call-site uniformity
    fn endpoint_ws_url(&self, endpoint: &Endpoint, path: &str, query: &str) -> Option<String> {
        let scheme = match endpoint.scheme {
            EndpointScheme::Http => "ws",
            EndpointScheme::Https | EndpointScheme::Wss => "wss",
            _ => return None,
        };
        let authority = authority(endpoint);
        let mut url = format!("{scheme}://{authority}{path}");
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        Some(url)
    }

    /// Map a forwarding failure onto the matching gateway error.
    fn map_forward_error(
        &self,
        e: ForwardError,
        target: &ResolvedTarget,
        path: &str,
    ) -> DomainError {
        let pc = self.problem_context(target, path);
        match e {
            ForwardError::Timeout { detail } => DomainError::TimeoutRequest {
                detail,
                context: Some(pc),
            },
            ForwardError::Connect { detail } => DomainError::LinkUnavailable {
                detail,
                context: Some(pc),
            },
            ForwardError::Stream { detail } => DomainError::StreamAborted {
                detail,
                context: Some(pc),
            },
            ForwardError::PayloadTooLarge { detail } => DomainError::PayloadTooLarge {
                detail,
                context: Some(pc),
            },
        }
    }

    /// The number of tracked rate-limit buckets (test helper).
    #[must_use]
    pub fn rate_buckets(&self) -> usize {
        self.rate_limiter.len()
    }

    /// Maximum accepted request body size in bytes (from the gear config).
    #[must_use]
    pub fn max_body_bytes(&self) -> usize {
        self.config.max_body_bytes
    }
}

/// Read the first `X-OAGW-Target-Host` header value.
fn target_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-oagw-target-host")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The 429 `RateLimitExceeded` problem for a rejected exchange.
fn rate_exceeded(
    target: &ResolvedTarget,
    req: &ProxyRequest,
    headers: &RateLimitHeaders,
    retry_after: u64,
) -> DomainError {
    DomainError::RateLimitExceeded {
        detail: format!(
            "rate limit exceeded for upstream '{}'",
            target.upstream.alias
        ),
        retry_after_seconds: retry_after,
        context: Some(ProblemContext {
            alias: Some(target.upstream.alias.clone()),
            path: Some(req.path.clone()),
            retry_after_seconds: Some(retry_after),
            rate_limit_limit: Some(headers.limit),
            rate_limit_remaining: Some(headers.remaining),
            rate_limit_reset: Some(headers.reset_epoch),
            ..ProblemContext::new()
        }),
    }
}

/// The first query parameter present in `query` but absent from `allowlist`
/// (case-insensitive), or `None` when every present parameter is allowed.
fn unknown_query_params(query: &str, allowlist: &[String]) -> Option<String> {
    for (key, _) in url::form_urlencoded::parse(query.as_bytes()) {
        if !allowlist
            .iter()
            .any(|a| a.eq_ignore_ascii_case(key.as_ref()))
        {
            return Some(key.into_owned());
        }
    }
    None
}

/// Build the outbound request header map from the inbound headers applying
/// the configured passthrough policy and `set`/`add`/`remove` rules. Routing
/// and hop-by-hop headers are always stripped.
fn make_outbound_headers(inbound: &HeaderMap, rules: &HeaderTransform) -> HeaderMap {
    let mut out = HeaderMap::new();
    match rules.passthrough {
        PassthroughMode::None => {}
        PassthroughMode::Allowlist => {
            for (name, value) in inbound {
                if is_stripped(name.as_str()) {
                    continue;
                }
                if rules
                    .passthrough_allowlist
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(name.as_str()))
                {
                    out.append(name, value.clone());
                }
            }
        }
        PassthroughMode::All => {
            for (name, value) in inbound {
                if !is_stripped(name.as_str()) {
                    out.append(name, value.clone());
                }
            }
        }
    }
    apply_side_transform(&mut out, rules);
    out
}

/// Apply a `set`/`add`/`remove` transform to a header map.
fn apply_side_transform(headers: &mut HeaderMap, tx: &HeaderTransform) {
    for (name, value) in &tx.set {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::try_from(name),
            http::header::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &tx.add {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::try_from(name),
            http::header::HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    for name in &tx.remove {
        headers.remove(name);
    }
}

fn is_stripped(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str()) || ROUTING_HEADERS.contains(&lower.as_str())
}

/// Remove hop-by-hop headers from an upstream response before it is streamed
/// back to the client (the transport re-frames the body).
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
}

/// Replace the `Host` header with the selected endpoint authority.
fn replace_host_header(headers: &mut HeaderMap, endpoint: &Endpoint) {
    if let Ok(value) = HeaderValue::from_str(&authority(endpoint)) {
        headers.insert(HOST, value);
    }
}

/// `host` or `host:port` (non-standard only).
fn authority(endpoint: &Endpoint) -> String {
    let host = endpoint.normalized_host();
    if endpoint.port == endpoint.scheme.default_port() {
        host
    } else {
        format!("{host}:{}", endpoint.port)
    }
}

/// `X-OAGW-Target-Host` format check: hostname or IP, no port / path /
/// special characters.
fn is_valid_target_host_value(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.contains(':') || value.contains('/') || value.contains(' ') {
        return false;
    }
    let normalized = value.trim_end_matches('.');
    if normalized.is_empty() {
        return false;
    }
    crate::domain::services::alias::is_valid_hostname(&normalized.to_ascii_lowercase())
        || crate::domain::services::alias::is_ipv4(normalized)
}

/// Map a plugin error onto a gateway error. The default mapping has no
/// request context; attach the minimal upstream context here.
fn map_plugin_error(err: PluginError) -> DomainError {
    err.into_domain_error()
}

fn plugin_missing(plugin_ref: &str, target: &ResolvedTarget) -> DomainError {
    DomainError::PluginNotFound {
        detail: format!("plugin '{plugin_ref}' is not available for execution"),
        context: Some(ProblemContext {
            upstream_id: Some(gts_helpers::upstream_resource_id(target.upstream.id)),
            alias: Some(target.upstream.alias.clone()),
            plugin_id: Some(plugin_ref.to_owned()),
            ..ProblemContext::new()
        }),
    }
}

/// Rebuild a gateway error from status/code/detail (used when a
/// `transform_error` mutated the error context).
fn gateway_error(status: u16, detail: String, context: ProblemContext) -> DomainError {
    match status {
        401 => DomainError::AuthFailed {
            detail,
            context: Some(context),
        },
        429 => DomainError::RateLimitExceeded {
            detail,
            retry_after_seconds: context.retry_after_seconds.unwrap_or(1),
            context: Some(context),
        },
        502 => DomainError::DownstreamError {
            detail,
            context: Some(context),
        },
        503 => DomainError::LinkUnavailable {
            detail,
            context: Some(context),
        },
        504 => DomainError::TimeoutRequest {
            detail,
            context: Some(context),
        },
        _ => DomainError::Validation {
            detail,
            context: Some(context),
        },
    }
}
