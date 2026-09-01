//! Data-plane service: request routing, plugin execution, rate limiting,
//! header transformation and the upstream call (ADR-0001..0003, 0007, 0009).
//!
//! Owns the proxy lifecycle for one inbound request and produces an
//! `axum::response::Response` (streaming SSE responses / WebSocket bridges
//! included).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::response::Response;
use futures_util::StreamExt;
use http::HeaderMap;
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tracing::{debug, warn};
use uuid::Uuid;

use toolkit_security::SecurityContext;

use super::alias::{compute_derived_alias, is_standard_port, is_valid_host, normalize_alias};
use super::dto::{
    AUTH_NOOP, AuthConfig, CorsConfig, Endpoint, HttpMatch, PassthroughMode, PathSuffixMode,
    PluginBinding, RateLimitConfig, RateLimitScope, Route, Upstream,
};
use super::error::{DomainError, PERM_PROXY_INVOKE, scope_allows};
use super::hierarchy::TenantHierarchy;
use super::merge::{
    effective_auth, effective_cors, effective_plugin_bindings, effective_rate_limits,
};
use super::plugin::{
    AuthPluginRegistry, ErrorContext, GuardDecision, GuardPluginRegistry, PluginError,
    RequestContext, ResponseContext, TransformPluginRegistry,
};
use super::ratelimit::{RateCheck, RateLimiterRegistry};
use super::repo::OagwRepository;
use crate::config::OagwConfig;
use crate::infra::proxy::{BoxBody, ProxyEngine, ProxyError, ProxyRequest, UpgradeOutcome};

/// Hop-by-hop headers removed on both directions (RFC 9110 §7.6.1).
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

/// Inbound headers preserved when the upstream's request `passthrough` mode
/// is `none`.
const SAFE_PASSTHROUGH: &[&str] = &[
    "accept",
    "accept-encoding",
    "accept-language",
    "content-type",
    "user-agent",
    // `x-request-id` is a gateway-managed tracing header: the request_id
    // transform plugin propagates it across the hop even when passthrough
    // is `none` (a bound plugin explicitly asks for it).
    "x-request-id",
];

const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Round-robin cursor for multi-endpoint explicit-alias upstreams.
#[derive(Default)]
struct RoundRobin(AtomicU64);

impl RoundRobin {
    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// A resolved alias shadow-chain (ROOT → SELECTED) plus the routed route.
struct Resolved {
    chain: Vec<Upstream>,
    route: Route,
}

impl Resolved {
    /// The selected (leaf) upstream of a resolved chain.
    ///
    /// `resolve` only constructs `Resolved` with a non-empty `chain`, so the
    /// expect tests an invariant the constructor guarantees.
    #[allow(clippy::expect_used)]
    fn selected(&self) -> &Upstream {
        self.chain.last().expect("chain non-empty")
    }
}

pub struct DataPlaneService {
    repo: Arc<dyn OagwRepository>,
    hierarchy: Arc<dyn TenantHierarchy>,
    auth_plugins: AuthPluginRegistry,
    guard_plugins: GuardPluginRegistry,
    transform_plugins: TransformPluginRegistry,
    engine: Arc<dyn ProxyEngine>,
    ratelimiter: RateLimiterRegistry,
    allow_http_upstream: bool,
    max_request_body_bytes: usize,
    rr: RoundRobin,
}

impl DataPlaneService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<dyn OagwRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        auth_plugins: AuthPluginRegistry,
        guard_plugins: GuardPluginRegistry,
        transform_plugins: TransformPluginRegistry,
        engine: Arc<dyn ProxyEngine>,
        config: &OagwConfig,
    ) -> Self {
        Self {
            repo,
            hierarchy,
            auth_plugins,
            guard_plugins,
            transform_plugins,
            engine,
            ratelimiter: RateLimiterRegistry::default(),
            allow_http_upstream: config.allow_http_upstream,
            // Config is a sane byte cap; clamp to the host pointer width.
            max_request_body_bytes: usize::try_from(config.max_request_body_bytes)
                .unwrap_or(usize::MAX),
            rr: RoundRobin::default(),
        }
    }

    // ---- public entry point ----------------------------------------------

    /// Execute the full proxy lifecycle for an inbound request.
    ///
    /// `suffix` is the request path portion that follows the upstream alias
    /// (e.g. `/v1/users` for `.../proxy/my-alias/v1/users`).  Route matching
    /// and upstream forwarding operate on that remainder.
    //
    // The lifecycle is a linear, documented pipeline of independent steps with
    // early returns at each stage; splitting it further would obscure the
    // ADR-0002 ordering the design mandates.
    #[allow(clippy::cognitive_complexity)]
    pub async fn proxy(
        &self,
        sctx: &SecurityContext,
        req: axum::extract::Request,
        alias: &str,
        suffix: &str,
    ) -> Response {
        let (parts, body) = req.into_parts();
        let method = parts.method.clone();
        let origin = parts
            .headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        // 1. CORS preflight short-circuits BEFORE the `proxy:invoke`
        //    permission check and the plugin pipeline (ADR-0004).  Tenant
        //    context is still present and effective CORS is resolved
        //    best-effort, but auth/guard/transform plugins never run.
        if let Some(preflight) = self
            .maybe_preflight(sctx, &parts, &method, alias, suffix)
            .await
        {
            return preflight;
        }

        // 2. Permission (skipped for preflight above).
        if !scope_allows(sctx.token_scopes(), PERM_PROXY_INVOKE) {
            return self
                .gateway_error(DomainError::PermissionDenied(PERM_PROXY_INVOKE.into()), &[])
                .await;
        }

        // 3. Tenant chain, alias resolution, route match.
        let request_path = format!("/{}", suffix.trim_start_matches('/'));
        let tenant = sctx.subject_tenant_id();
        let chain = self.hierarchy.ancestor_chain(sctx, tenant).await;
        let resolved = match self.resolve(&chain, alias, &method, &request_path) {
            Ok(r) => r,
            Err(e) => return self.gateway_error(e, &[]).await,
        };
        let route = match Self::match_route(&resolved, &request_path, &method) {
            Ok(r) => r,
            Err(e) => return self.gateway_error(e, &[]).await,
        };

        // 4. Effective config across the shadow-chain.
        let chain_matches: Vec<&Upstream> = resolved.chain.iter().collect();
        let effective_plugins = effective_plugin_bindings(&chain_matches, &route.plugins);
        let effective_auth = effective_auth(&chain_matches);
        let effective_limits = effective_rate_limits(&chain_matches, route.rate_limit.as_ref());
        let effective_cors = effective_cors(&chain_matches, route.cors.as_ref());

        // 4b. Query parameter allowlist (DESIGN.md §Validation): reject any
        // query parameter that is not in `match.http.query_allowlist`
        // (an empty allowlist admits none).
        if let Some(http_match) = route.match_config.http.as_ref()
            && let Err(e) = check_query_allowlist(http_match, &parts.uri)
        {
            return self.gateway_error(e, &effective_plugins).await;
        }

        // 5. CORS for actual requests.
        if let Some(cors) = &effective_cors
            && cors.enabled
            && let Err(e) = Self::check_actual_cors(cors, &parts.headers, &method)
        {
            return self.gateway_error(e, &effective_plugins).await;
        }

        // 6. Auth plugin (single effective auth choice).
        let mut current_headers = parts.headers.clone();
        let auth_type = effective_auth
            .plugin_type
            .as_deref()
            .filter(|_| has_real_auth(&effective_auth))
            .map(str::to_owned);
        if let Some(plugin_type) = auth_type {
            let Some(plugin) = self.auth_plugins.get(&plugin_type) else {
                return self
                    .gateway_error(DomainError::PluginNotFound(plugin_type), &effective_plugins)
                    .await;
            };
            let mut rctx = RequestContext {
                config: effective_auth.config.clone(),
                method: method.clone(),
                uri: parts.uri.clone(),
                headers: current_headers.clone(),
                security_context: sctx.clone(),
            };
            let result = plugin.authenticate(&mut rctx).await;
            match result {
                Ok(()) => current_headers = rctx.headers.clone(),
                Err(e) => {
                    return self
                        .gateway_error(
                            match e {
                                PluginError::AuthFailed(msg) => {
                                    DomainError::AuthenticationFailed(msg)
                                }
                                PluginError::Config(msg) | PluginError::Internal(msg) => {
                                    DomainError::Internal(msg)
                                }
                                PluginError::Reject { .. } => DomainError::Internal(
                                    "auth plugin rejection unsupported".into(),
                                ),
                            },
                            &effective_plugins,
                        )
                        .await;
                }
            }
        }

        // 7. Guards (request phase).
        let guards = self.resolve_guards(&effective_plugins);
        for guard in &guards {
            let ctx = RequestContext {
                config: guard.1.clone(),
                method: method.clone(),
                uri: parts.uri.clone(),
                headers: current_headers.clone(),
                security_context: sctx.clone(),
            };
            match guard.0.guard_request(&ctx).await {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject {
                    status,
                    error_code,
                    detail,
                }) => return Self::plugin_reject(status, &error_code, &detail),
                Err(e) => {
                    return self
                        .gateway_error(Self::plugin_failure(e), &effective_plugins)
                        .await;
                }
            }
        }

        // 8. Transform plugins (request phase).
        let transforms = self.resolve_transforms(&effective_plugins);
        let mut request_id: Option<String> = None;
        for transform in &transforms {
            let mut rctx = RequestContext {
                config: transform.1.clone(),
                method: method.clone(),
                uri: parts.uri.clone(),
                headers: current_headers.clone(),
                security_context: sctx.clone(),
            };
            if let Err(e) = transform.0.transform_request(&mut rctx).await {
                return self
                    .gateway_error(Self::plugin_failure(e), &effective_plugins)
                    .await;
            }
            if let Some(id) = rctx.headers.get("x-request-id") {
                request_id = id.to_str().ok().map(str::to_owned);
            }
            current_headers = rctx.headers;
        }

        // 9. Endpoint selection (ADR-0001 Target-Host matrix).
        let endpoint = match self.select_endpoint(resolved.selected(), &current_headers) {
            Ok(e) => e,
            Err(e) => return self.gateway_error(e, &effective_plugins).await,
        };

        // 10. Rate limits.
        let checks = match self.check_rate_limits(&effective_limits, sctx, &parts, &route) {
            Ok(c) => c,
            Err(e) => return self.gateway_error(e, &effective_plugins).await,
        };

        // 11. Buffer + validate the request body (bounded by config cap).
        let body = match self.buffer_body(&parts.headers, body).await {
            Ok(b) => b,
            Err(e) => return self.gateway_error(e, &effective_plugins).await,
        };

        // 12. WebSocket upgrade path.
        let wants_upgrade = requests_upgrade(&current_headers);
        if wants_upgrade {
            return self
                .proxy_ws(
                    parts,
                    &resolved,
                    &current_headers,
                    &endpoint,
                    &effective_plugins,
                    request_id,
                    effective_cors.as_ref(),
                    origin.as_deref(),
                    &request_path,
                )
                .await;
        }

        // 13. Normal upstream call.
        let (status, resp_headers, resp_body) = match self
            .build_and_call(
                &parts,
                &endpoint,
                &request_path,
                &current_headers,
                &resolved,
                body,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return self.gateway_error(e, &effective_plugins).await,
        };

        // 14. Response post-processing.
        self.emit_response(
            status,
            resp_headers,
            resp_body,
            &effective_plugins,
            request_id,
            &checks,
            &effective_limits,
            effective_cors.as_ref(),
            origin.as_deref(),
        )
        .await
    }

    // ---- CORS ------------------------------------------------------------

    /// Detect and answer a CORS preflight (OPTIONS + Origin + ACRM) with a
    /// permissive 204 echo *before* the `proxy:invoke` permission check and
    /// the plugin pipeline (ADR-0004: preflight bypasses per-request
    /// auth/plugin checks by design).  The tenant context is available, so
    /// the effective CORS config is resolved best-effort; a failed/absent
    /// resolution is tolerated and the preflight stays permissive.
    async fn maybe_preflight(
        &self,
        sctx: &SecurityContext,
        parts: &axum::http::request::Parts,
        method: &http::Method,
        alias: &str,
        suffix: &str,
    ) -> Option<Response> {
        if *method != http::Method::OPTIONS {
            return None;
        }
        let headers = &parts.headers;
        if !headers.contains_key(http::header::ORIGIN) {
            return None;
        }
        let _ = headers.get("access-control-request-method")?;
        let _effective = self.try_effective_cors(sctx, alias, suffix).await;

        let origin = headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("*")
            .to_owned();
        let acrm = headers
            .get("access-control-request-method")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("GET")
            .to_owned();

        // Build the permissive 204 echo without a fallible builder: every
        // value is either a validated header string or a static literal.
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = http::StatusCode::NO_CONTENT;
        resp.headers_mut().insert(
            "access-control-allow-origin",
            echoed_header_value(&origin, "*"),
        );
        resp.headers_mut().insert(
            "access-control-allow-methods",
            echoed_header_value(&acrm, "GET"),
        );
        resp.headers_mut().insert(
            "access-control-max-age",
            http::HeaderValue::from_static("86400"),
        );
        resp.headers_mut().insert(
            http::header::VARY,
            http::HeaderValue::from_static(
                "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
            ),
        );
        if let Some(acrh) = headers.get("access-control-request-headers") {
            resp.headers_mut()
                .insert("access-control-allow-headers", acrh.clone());
        }
        Some(resp)
    }

    /// Best-effort resolution of the effective CORS config for a preflight.
    /// Any resolution failure (unknown alias, no route, disabled chain) maps
    /// to `None` — preflights never hard-fail.
    async fn try_effective_cors(
        &self,
        sctx: &SecurityContext,
        alias: &str,
        suffix: &str,
    ) -> Option<CorsConfig> {
        let request_path = format!("/{}", suffix.trim_start_matches('/'));
        let tenant = sctx.subject_tenant_id();
        let chain = self.hierarchy.ancestor_chain(sctx, tenant).await;
        let resolved = self
            .resolve(&chain, alias, &http::Method::GET, &request_path)
            .ok()?;
        let route = Self::match_route(&resolved, &request_path, &http::Method::GET).ok()?;
        let chain_matches: Vec<&Upstream> = resolved.chain.iter().collect();
        effective_cors(&chain_matches, route.cors.as_ref())
    }

    fn check_actual_cors(
        cors: &CorsConfig,
        headers: &HeaderMap,
        method: &http::Method,
    ) -> Result<(), DomainError> {
        let Some(origin) = headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        else {
            return Ok(()); // no Origin → not a CORS request
        };
        let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
        if !wildcard && !cors.allowed_origins.iter().any(|o| o == origin) {
            return Err(DomainError::CorsOriginNotAllowed(origin.to_owned()));
        }
        let method_str = method.as_str().to_uppercase();
        if !cors
            .allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&method_str))
        {
            return Err(DomainError::CorsMethodNotAllowed(method_str));
        }
        Ok(())
    }

    // ---- resolution ------------------------------------------------------

    fn resolve(
        &self,
        chain: &[Uuid],
        alias: &str,
        method: &http::Method,
        request_path: &str,
    ) -> Result<Resolved, DomainError> {
        let alias = normalize_alias(alias);
        let mut matches: Vec<Upstream> = Vec::new();
        for tenant in chain {
            if let Some(up) = self.repo.find_upstream_by_alias(*tenant, &alias) {
                matches.push(up);
            }
        }
        let selected = match matches.last() {
            Some(up) => up.clone(),
            None => return Err(DomainError::RouteNotFound { host: alias }),
        };
        // Shadow-chain semantics: ANY chain member matching the alias that is
        // disabled (ancestor or leaf) makes the alias unresolvable (503).
        // A descendant can never shadow back on a disabled ancestor.
        if let Some(disabled) = matches.iter().find(|u| !u.enabled) {
            return Err(DomainError::UpstreamDisabled(format!(
                "upstream '{}' (alias '{alias}') is disabled",
                disabled.alias
            )));
        }
        // Find the matching route for the SELECTED upstream.
        let routes = self.repo.list_routes(selected.tenant_id);
        let Some(route) = Self::best_route(&routes, selected.id, method, request_path) else {
            return Err(DomainError::RouteNotFound { host: alias });
        };
        Ok(Resolved {
            chain: matches,
            route,
        })
    }

    /// Deterministic route selection: among enabled routes for the upstream
    /// that admit the request method AND whose `path` prefix-matches the
    /// proxy request path, pick the longest `path`; ties broken by higher
    /// `priority` (DESIGN.md route determinism).  An empty `methods` list
    /// admits any method.
    fn best_route(
        routes: &[Route],
        upstream_id: Uuid,
        method: &http::Method,
        request_path: &str,
    ) -> Option<Route> {
        routes
            .iter()
            .filter(|r| {
                r.enabled
                    && r.upstream_id == upstream_id
                    && route_method_allows(r, method)
                    && Self::route_path_prefix_matches(r, request_path)
            })
            .cloned()
            .max_by_key(|r| {
                let plen = match &r.match_config.http {
                    Some(h) => h.path.len(),
                    None => 0,
                };
                (plen, r.priority)
            })
    }

    /// Prefix rule shared by route selection and post-selection validation:
    /// the route's HTTP path must be a non-empty prefix of the proxy request
    /// path (a root `/` path is rejected as empty).  gRPC routes never match
    /// the HTTP data plane.
    fn route_path_prefix_matches(r: &Route, request_path: &str) -> bool {
        let Some(h) = &r.match_config.http else {
            return false;
        };
        let base = h.path.trim_end_matches('/');
        !base.is_empty() && request_path.starts_with(base)
    }

    fn match_route(
        resolved: &Resolved,
        request_path: &str,
        method: &http::Method,
    ) -> Result<Route, DomainError> {
        let route = resolved.route.clone();
        let Some(http_match) = &route.match_config.http else {
            // gRPC routes are not matched by the HTTP data plane.
            return Err(DomainError::RouteNotFound {
                host: resolved.selected().alias.clone(),
            });
        };
        let base = http_match.path.trim_end_matches('/');
        if base.is_empty() || !request_path.starts_with(base) {
            return Err(DomainError::RouteNotFound {
                host: resolved.selected().alias.clone(),
            });
        }
        // Method must be allowed (empty list = any).
        if !http_match.methods.is_empty()
            && !http_match
                .methods
                .iter()
                .any(|m| m.as_str() == method.as_str())
        {
            return Err(DomainError::RouteNotFound {
                host: resolved.selected().alias.clone(),
            });
        }
        // Path suffix (beyond the route base) semantics.
        let suffix = &request_path[base.len()..];
        if !suffix.is_empty() && http_match.path_suffix_mode == PathSuffixMode::Disabled {
            return Err(DomainError::Validation(format!(
                "path suffix '{suffix}' is not allowed for route '{}' (path_suffix_mode=disabled)",
                http_match.path
            )));
        }
        Ok(route)
    }

    // ---- plugins ---------------------------------------------------------

    fn resolve_guards(
        &self,
        bindings: &[PluginBinding],
    ) -> Vec<(Arc<dyn super::plugin::GuardPlugin>, Value)> {
        bindings
            .iter()
            .filter_map(|b| {
                self.guard_plugins
                    .get(&b.plugin_ref)
                    .map(|p| (p, b.config.clone()))
            })
            .collect()
    }

    fn resolve_transforms(
        &self,
        bindings: &[PluginBinding],
    ) -> Vec<(Arc<dyn super::plugin::TransformPlugin>, Value)> {
        bindings
            .iter()
            .filter_map(|b| {
                self.transform_plugins
                    .get(&b.plugin_ref)
                    .map(|p| (p, b.config.clone()))
            })
            .collect()
    }

    fn plugin_failure(e: PluginError) -> DomainError {
        match e {
            PluginError::Config(msg) | PluginError::Internal(msg) => DomainError::Internal(msg),
            PluginError::AuthFailed(msg) => DomainError::AuthenticationFailed(msg),
            PluginError::Reject {
                status,
                error_code,
                detail,
            } => DomainError::Internal(format!("plugin rejected {status} {error_code}: {detail}")),
        }
    }

    fn plugin_reject(status: u16, error_code: &str, detail: &str) -> Response {
        let body = serde_json::json!({
            "type": "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            "title": "Plugin Rejection",
            "status": status,
            "code": error_code,
            "detail": detail,
        });
        let mut resp = Response::new(Body::from(serde_json::to_vec(&body).unwrap_or_default()));
        *resp.status_mut() =
            http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_REQUEST);
        resp.headers_mut().insert(
            "content-type",
            http::HeaderValue::from_static("application/problem+json"),
        );
        resp.headers_mut().insert(
            "x-oagw-error-source",
            http::HeaderValue::from_static("gateway"),
        );
        resp
    }

    // ---- endpoint selection (Target-Host matrix) -------------------------

    fn select_endpoint(
        &self,
        upstream: &Upstream,
        headers: &HeaderMap,
    ) -> Result<Endpoint, DomainError> {
        let endpoints = &upstream.server.endpoints;
        let raw_target = headers
            .get(TARGET_HOST_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Strict X-OAGW-Target-Host validation (ADR-0007): the value must be a
        // bare RFC 1123 hostname or IP literal — no port, path or special
        // characters.  Malformed → invalid_target_host (400) *before* any
        // endpoint matching.  Well-formed but non-matching → unknown_target_host.
        let explicit_target: Option<String> = match raw_target {
            Some(t) => {
                if !is_valid_host(t) {
                    return Err(DomainError::InvalidTargetHost(t.to_owned()));
                }
                Some(normalize_alias(t))
            }
            None => None,
        };

        let derived = compute_derived_alias(
            &endpoints
                .iter()
                .map(|e| (e.scheme.clone(), e.host.clone(), e.port))
                .collect::<Vec<_>>(),
        );
        // Valid hosts are the bare endpoint hostnames (extension field).
        let valid_hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();
        match explicit_target {
            Some(target) => {
                for ep in endpoints {
                    if matches_endpoint(ep, &target) {
                        return Ok(ep.clone());
                    }
                }
                Err(DomainError::UnknownTargetHost {
                    invalid_value: target,
                    valid_hosts,
                })
            }
            None => {
                if derived.as_deref() == Some(upstream.alias.as_str()) {
                    // Auto-derived common-suffix pool → Target-Host REQUIRED.
                    Err(DomainError::MissingTargetHost { valid_hosts })
                } else {
                    // Explicit alias over a pool → round-robin. The monotonic
                    // counter never exceeds a usize on 64-bit hosts; clamp for
                    // 32-bit safety (the counter wrapping only shifts order).
                    let idx =
                        usize::try_from(self.rr.next()).unwrap_or(usize::MAX) % endpoints.len();
                    Ok(endpoints[idx].clone())
                }
            }
        }
    }

    // ---- rate limiting ---------------------------------------------------

    fn check_rate_limits(
        &self,
        limits: &[RateLimitConfig],
        sctx: &SecurityContext,
        parts: &axum::http::request::Parts,
        route: &Route,
    ) -> Result<Vec<RateCheck>, DomainError> {
        let mut checks = Vec::new();
        for cfg in limits {
            let key = scope_key(cfg, sctx, parts, route);
            let check = self.ratelimiter.check(cfg, &key, cfg.cost);
            if !check.allowed {
                return Err(DomainError::RateLimitExceeded {
                    retry_after_secs: check.retry_after_secs,
                    limit: check.limit,
                    remaining: check.remaining,
                    reset_at_unix: check.reset_at_unix,
                });
            }
            checks.push(check);
        }
        Ok(checks)
    }

    // ---- body -------------------------------------------------------------

    /// Buffer the request body, validating the framing headers first:
    /// `Content-Length` must be a valid non-negative integer (a declared size
    /// over the cap is rejected with 413 *before* any body is consumed) and
    /// `Transfer-Encoding` must be exactly `chunked`.  A final length
    /// mismatch between the declared `Content-Length` and the bytes actually
    /// received is also a 400 validation error.
    async fn buffer_body(&self, headers: &HeaderMap, body: Body) -> Result<Vec<u8>, DomainError> {
        let declared = if let Some(cl) = headers.get(http::header::CONTENT_LENGTH) {
            let s = cl
                .to_str()
                .map_err(|_| invalid_body("content-length must be a valid integer"))?;
            let n: u64 = s
                .parse()
                .map_err(|_| invalid_body("content-length must be a valid integer"))?;
            if n > self.max_request_body_bytes as u64 {
                return Err(DomainError::PayloadTooLarge(format!(
                    "declared content-length {n} exceeds the {} byte cap",
                    self.max_request_body_bytes
                )));
            }
            Some(n)
        } else {
            None
        };
        if let Some(te) = headers.get(http::header::TRANSFER_ENCODING) {
            let s = te
                .to_str()
                .map_err(|_| invalid_body("transfer-encoding must be 'chunked'"))?;
            if !s.eq_ignore_ascii_case("chunked") {
                return Err(invalid_body(
                    "transfer-encoding other than 'chunked' is not supported",
                ));
            }
        }

        let mut out = Vec::new();
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if out.len() + bytes.len() > self.max_request_body_bytes {
                        return Err(DomainError::PayloadTooLarge(format!(
                            "request body exceeds {} bytes",
                            self.max_request_body_bytes
                        )));
                    }
                    out.extend_from_slice(&bytes);
                }
                Err(e) => return Err(DomainError::ProtocolError(format!("body error: {e}"))),
            }
        }
        if let Some(declared) = declared
            && (out.len() as u64) != declared
        {
            return Err(invalid_body(
                "content-length does not match the received body size",
            ));
        }
        Ok(out)
    }

    // ---- upstream call ----------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn build_and_call(
        &self,
        parts: &axum::http::request::Parts,
        endpoint: &Endpoint,
        request_path: &str,
        headers: &HeaderMap,
        resolved: &Resolved,
        body: Vec<u8>,
    ) -> Result<(http::StatusCode, HeaderMap, BoxBody), DomainError> {
        let scheme = endpoint.scheme.as_str();
        if matches!(scheme, "http" | "ws") && !self.allow_http_upstream {
            return Err(DomainError::Validation(format!(
                "scheme '{scheme}' is not allowed (allow_http_upstream=false)"
            )));
        }
        let http_scheme = if matches!(scheme, "https" | "wss" | "grpc") {
            "https"
        } else {
            "http"
        };
        let authority = endpoint_authority(endpoint);
        let mut target_uri = format!("{http_scheme}://{authority}{request_path}");
        if let Some(q) = parts.uri.query()
            && !q.is_empty()
        {
            target_uri.push('?');
            target_uri.push_str(q);
        }
        let uri: http::Uri = target_uri
            .parse()
            .map_err(|e| DomainError::Internal(format!("bad target uri: {e}")))?;

        let upstream_headers = build_upstream_headers(
            &parts.headers,
            headers,
            resolved.selected(),
            &authority,
            resolved.selected().headers.request.passthrough,
        );

        let req = ProxyRequest {
            method: parts.method.clone(),
            uri,
            headers: upstream_headers,
            body: Body::from(body)
                .map_err(crate::infra::proxy::into_box_error)
                .boxed_unsync(),
        };
        let resp = self.engine.call(req).await.map_err(Self::map_proxy_error)?;
        Ok((resp.status, resp.headers, resp.body))
    }

    fn map_proxy_error(e: ProxyError) -> DomainError {
        match e {
            ProxyError::RequestTimeout(_) => DomainError::RequestTimeout,
            ProxyError::Connect(err) => {
                let msg = err.to_string();
                debug!(error = %msg, "upstream connect failure");
                DomainError::LinkUnavailable(msg)
            }
            ProxyError::Stream(msg) => DomainError::StreamAborted(msg),
            ProxyError::Upgrade(msg) | ProxyError::Body(msg) => DomainError::ProtocolError(msg),
            ProxyError::InvalidUri(msg) => DomainError::Validation(msg),
        }
    }

    // ---- websocket --------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn proxy_ws(
        &self,
        parts: axum::http::request::Parts,
        resolved: &Resolved,
        current_headers: &HeaderMap,
        endpoint: &Endpoint,
        effective_plugins: &[PluginBinding],
        request_id: Option<String>,
        cors: Option<&CorsConfig>,
        origin: Option<&str>,
        request_path: &str,
    ) -> Response {
        // Connect over http(s) — the engine's connector only supports those
        // schemes; the WebSocket upgrade handshake itself is carried in the
        // request headers (Connection: Upgrade + Upgrade: websocket).
        let scheme = if matches!(endpoint.scheme.as_str(), "https" | "wss") {
            "https"
        } else {
            "http"
        };
        let authority = endpoint_authority(endpoint);
        // The upgrade handshake must target the route-relative (rewritten)
        // path — the same `request_path` the plain-HTTP path proxies — not the
        // full client-facing URI (`/api/oagw/v1/proxy/{alias}/...`).
        let mut target = format!("{scheme}://{authority}{request_path}");
        if let Some(q) = parts.uri.query()
            && !q.is_empty()
        {
            target.push('?');
            target.push_str(q);
        }
        let uri: http::Uri = match target.parse() {
            Ok(u) => u,
            Err(e) => {
                return self
                    .gateway_error(
                        DomainError::Internal(format!("uri: {e}")),
                        effective_plugins,
                    )
                    .await;
            }
        };

        // Preserve every client header that websocket handshakes need
        // (Sec-WebSocket-Key/Version/Origin etc.) — hop-by-hop stripping only.
        let mut req_headers = build_upstream_headers(
            &parts.headers,
            current_headers,
            resolved.selected(),
            &authority,
            PassthroughMode::All,
        );
        req_headers.insert(
            http::header::CONNECTION,
            http::HeaderValue::from_static("Upgrade"),
        );
        req_headers.insert(
            http::header::UPGRADE,
            http::HeaderValue::from_static("websocket"),
        );
        let req = ProxyRequest {
            method: parts.method.clone(),
            uri,
            headers: req_headers,
            body: Body::empty()
                .map_err(crate::infra::proxy::into_box_error)
                .boxed_unsync(),
        };

        match self.engine.upgrade(req).await {
            Ok(UpgradeOutcome::Upgraded {
                headers: up_headers,
                conn: upstream_conn,
            }) => {
                // Commit the server-side upgrade for the client connection.
                //
                // IMPORTANT: `hyper::upgrade::on` (server side) only resolves
                // AFTER hyper has sent the 101 response and switched the
                // connection — awaiting it inline here would deadlock the
                // handler before the response is ever produced.  The correct
                // pattern is to return the 101 immediately and await the
                // client upgrade in a background task.
                let mut request = axum::extract::Request::from_parts(parts, Body::empty());
                tokio::spawn(async move {
                    match hyper::upgrade::on(&mut request).await {
                        Ok(downstream) => {
                            let a = TokioIo::new(downstream);
                            let b = TokioIo::new(upstream_conn);
                            if let Err(e) = crate::infra::proxy::pump_bidirectional(a, b).await {
                                debug!(error = %e, "websocket bridge closed");
                            }
                        }
                        Err(e) => {
                            debug!(error = %e, "client upgrade failed while bridging");
                        }
                    }
                });
                // Forward the upstream's 101 response headers verbatim —
                // `Sec-WebSocket-Accept` proves to the client that the
                // handshake completed against its key (RFC 6455 §4.2.2) and a
                // real WebSocket implementation rejects a 101 without it.
                // Connection/Upgrade are re-asserted last so the response is
                // a valid upgrade reply even if the upstream omitted them.
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = http::StatusCode::SWITCHING_PROTOCOLS;
                resp.headers_mut().extend(up_headers);
                resp.headers_mut().insert(
                    http::header::CONNECTION,
                    http::HeaderValue::from_static("Upgrade"),
                );
                resp.headers_mut().insert(
                    http::header::UPGRADE,
                    http::HeaderValue::from_static("websocket"),
                );
                resp
            }
            Ok(UpgradeOutcome::Response(resp)) => {
                self.emit_response(
                    resp.status,
                    resp.headers,
                    resp.body,
                    effective_plugins,
                    request_id,
                    &[],
                    &[],
                    cors,
                    origin,
                )
                .await
            }
            Err(e) => {
                self.gateway_error(Self::map_proxy_error(e), effective_plugins)
                    .await
            }
        }
    }

    // ---- response handling ------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn emit_response(
        &self,
        status: http::StatusCode,
        headers: HeaderMap,
        body: BoxBody,
        effective_plugins: &[PluginBinding],
        request_id: Option<String>,
        checks: &[RateCheck],
        limits: &[RateLimitConfig],
        cors: Option<&CorsConfig>,
        origin: Option<&str>,
    ) -> Response {
        let mut headers = headers;
        strip_hop_by_hop(&mut headers);
        headers.remove("content-length"); // re-chunked streaming

        // CORS response headers for a permitted actual request (ADR-0004):
        // `Access-Control-Allow-Origin` echoes the request origin (or `*`
        // when no credentials), expose-headers and allow-credentials per the
        // effective config, and `Vary: Origin` is ALWAYS present — even with
        // CORS disabled — to prevent cache poisoning.
        apply_cors_response_headers(&mut headers, cors, origin);

        // Transform (response) plugins — e.g. request_id echo.
        let transforms = self.resolve_transforms(effective_plugins);
        for transform in &transforms {
            let mut rctx = ResponseContext {
                config: transform.1.clone(),
                status,
                headers: headers.clone(),
                request_id: request_id.clone(),
            };
            if let Ok(()) = transform.0.transform_response(&mut rctx).await {
                headers = rctx.headers;
            }
        }

        // Guard (response) plugins — a rejection degrades to passthrough
        // (documented MVP behavior: response errors are observational).
        let guards = self.resolve_guards(effective_plugins);
        for guard in &guards {
            let ctx = ResponseContext {
                config: guard.1.clone(),
                status,
                headers: headers.clone(),
                request_id: request_id.clone(),
            };
            if let Ok(GuardDecision::Reject {
                error_code, detail, ..
            }) = guard.0.guard_response(&ctx).await
            {
                warn!(%error_code, %detail, "response guard rejection (passthrough)");
            }
        }

        // Rate-limit response headers (strictest limiter).
        if limits.iter().any(|l| l.response_headers)
            && let Some(check) = checks.iter().min_by_key(|c| c.remaining)
        {
            headers.insert("x-ratelimit-limit", num_header_value(check.limit));
            headers.insert("x-ratelimit-remaining", num_header_value(check.remaining));
            headers.insert("x-ratelimit-reset", num_header_value(check.reset_at_unix));
        }

        // ADR-0007: proxied responses are attributed to the upstream.
        headers.insert(
            "x-oagw-error-source",
            http::HeaderValue::from_static("upstream"),
        );

        let mut resp = Response::new(Body::new(body));
        *resp.status_mut() = status;
        *resp.headers_mut() = headers;
        resp
    }

    /// Render a gateway-side error as an RFC 9457 problem document with
    /// `X-OAGW-Error-Source: gateway`.
    ///
    /// Bound transform plugins get their `transform_error` hook run on every
    /// gateway error (ADR-0002: Transform(response/error) is the last plugin
    /// phase) and may rewrite the problem `type` and HTTP status.  Extension
    /// fields use `snake_case` (ADR-0007).
    async fn gateway_error(&self, e: DomainError, bindings: &[PluginBinding]) -> Response {
        let mut error_type = e.gts_id().to_owned();
        let mut effective_status = e.status();

        let transforms = self.resolve_transforms(bindings);
        for transform in &transforms {
            let mut ctx = ErrorContext {
                config: transform.1.clone(),
                error_type: error_type.clone(),
                status: effective_status,
            };
            if transform.0.transform_error(&mut ctx).await.is_ok() {
                error_type = ctx.error_type;
                effective_status = ctx.status;
            }
        }

        let mut body = serde_json::json!({
            "type": error_type,
            "title": e.title(),
            "status": effective_status,
            "detail": e.to_string(),
        });
        match &e {
            DomainError::MissingTargetHost { valid_hosts } => {
                body["valid_hosts"] = serde_json::json!(valid_hosts);
            }
            DomainError::InvalidTargetHost(invalid_value) => {
                body["invalid_value"] = serde_json::json!(invalid_value);
            }
            DomainError::UnknownTargetHost {
                invalid_value,
                valid_hosts,
            } => {
                body["invalid_value"] = serde_json::json!(invalid_value);
                body["valid_hosts"] = serde_json::json!(valid_hosts);
            }
            DomainError::RateLimitExceeded {
                retry_after_secs,
                limit,
                remaining,
                reset_at_unix,
            } => {
                body["retry_after_secs"] = serde_json::json!(retry_after_secs);
                body["limit"] = serde_json::json!(limit);
                body["remaining"] = serde_json::json!(remaining);
                body["reset_at_unix"] = serde_json::json!(reset_at_unix);
            }
            DomainError::PluginInUse {
                plugin_id,
                upstreams,
                routes,
            } => {
                body["plugin_id"] = serde_json::json!(plugin_id);
                body["referenced_by"] = serde_json::json!({
                    "upstreams": upstreams,
                    "routes": routes,
                });
            }
            _ => {}
        }

        let mut resp = Response::new(Body::from(serde_json::to_vec(&body).unwrap_or_default()));
        *resp.status_mut() = http::StatusCode::from_u16(effective_status)
            .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
        resp.headers_mut().insert(
            "content-type",
            http::HeaderValue::from_static("application/problem+json"),
        );
        resp.headers_mut().insert(
            "x-oagw-error-source",
            http::HeaderValue::from_static("gateway"),
        );
        resp.headers_mut()
            .insert(http::header::VARY, http::HeaderValue::from_static("Origin"));
        if let DomainError::RateLimitExceeded {
            retry_after_secs,
            limit,
            remaining,
            reset_at_unix,
        } = &e
        {
            resp.headers_mut()
                .insert("retry-after", num_header_value(retry_after_secs));
            resp.headers_mut()
                .insert("x-ratelimit-limit", num_header_value(limit));
            resp.headers_mut()
                .insert("x-ratelimit-remaining", num_header_value(remaining));
            resp.headers_mut()
                .insert("x-ratelimit-reset", num_header_value(reset_at_unix));
        }
        resp
    }
}

// ---- free helpers --------------------------------------------------------

fn has_real_auth(auth: &AuthConfig) -> bool {
    auth.plugin_type
        .as_deref()
        .is_some_and(|t| !t.is_empty() && t != AUTH_NOOP)
}

/// Build a `HeaderValue` from an integer-derived string (rate-limit headers).
///
/// Decimal integer formatting never produces octets invalid in a header
/// value, so the error arm is unreachable; the fallback keeps the function
/// total.
fn num_header_value(v: impl std::fmt::Display) -> http::HeaderValue {
    http::HeaderValue::from_str(&v.to_string())
        .unwrap_or_else(|_| http::HeaderValue::from_static("0"))
}

/// Re-encode a string that already round-tripped a `HeaderValue` (e.g. a
/// CORS `Origin` echo). `to_str` only yields valid header text, so the error
/// arm is unreachable; `fallback` keeps the function total.
fn echoed_header_value(s: &str, fallback: &'static str) -> http::HeaderValue {
    http::HeaderValue::from_str(s).unwrap_or_else(|_| http::HeaderValue::from_static(fallback))
}

/// Reject request query parameters that are not white-listed by the route's
/// `query_allowlist` (DESIGN.md validation rules — an empty allowlist admits
/// none).  Allowed parameters are forwarded as-is by the proxy.
fn check_query_allowlist(http_match: &HttpMatch, uri: &http::Uri) -> Result<(), DomainError> {
    if http_match.query_allowlist.is_empty() {
        // Empty allowlist ⇒ no query parameters are admitted.
        if uri.query().is_some_and(|q| !q.is_empty()) {
            return Err(DomainError::Validation(
                "query parameters are not allowed by this route (query_allowlist is empty)"
                    .to_owned(),
            ));
        }
        return Ok(());
    }
    let Some(raw) = uri.query() else {
        return Ok(());
    };
    if raw.is_empty() {
        return Ok(());
    }
    let allowed: Vec<&str> = http_match
        .query_allowlist
        .iter()
        .map(std::string::String::as_str)
        .collect();
    for (key, _) in form_urlencoded::parse(raw.as_bytes()) {
        let key = key.as_ref();
        if !allowed.contains(&key) {
            return Err(DomainError::Validation(format!(
                "query parameter '{key}' is not allowed by the route's query_allowlist"
            )));
        }
    }
    Ok(())
}

fn requests_upgrade(headers: &HeaderMap) -> bool {
    let connection_upgrade = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    connection_upgrade && headers.contains_key(http::header::UPGRADE)
}

/// Whether a route's HTTP match admits the request method (empty `methods`
/// list = any method).
fn route_method_allows(r: &Route, method: &http::Method) -> bool {
    match &r.match_config.http {
        Some(h) => h.methods.is_empty() || h.methods.iter().any(|m| m.as_str() == method.as_str()),
        None => false,
    }
}

/// Validation error for request framing (content-length / transfer-encoding).
fn invalid_body(msg: &str) -> DomainError {
    DomainError::Validation(msg.to_owned())
}

/// Header names nominated by the `Connection` header's value list (RFC 9110
/// §7.6.1) — e.g. `Connection: keep-alive, X-Foo` also strips `x-foo`.
fn connection_named_headers(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Add `Access-Control-Allow-*` + always-on `Vary: Origin` to a proxied
/// response (ADR-0004 actual-request handling).  `Vary: Origin` is emitted
/// unconditionally — even when CORS is disabled — to prevent cache poisoning.
fn apply_cors_response_headers(
    headers: &mut HeaderMap,
    cors: Option<&CorsConfig>,
    origin: Option<&str>,
) {
    append_vary(headers, "Origin");
    let Some(cors) = cors else {
        return;
    };
    if !cors.enabled {
        return;
    }
    let Some(origin) = origin else {
        return;
    };
    let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
    if cors.allow_credentials {
        // Credentials require the exact origin echoed (never `*`).
        if let Ok(v) = http::HeaderValue::from_str(origin) {
            headers.insert("access-control-allow-origin", v);
        }
    } else if wildcard {
        headers.insert(
            "access-control-allow-origin",
            http::HeaderValue::from_static("*"),
        );
    } else if let Ok(v) = http::HeaderValue::from_str(origin) {
        headers.insert("access-control-allow-origin", v);
    }
    if !cors.expose_headers.is_empty()
        && let Ok(v) = http::HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert("access-control-expose-headers", v);
    }
    if cors.allow_credentials {
        headers.insert(
            "access-control-allow-credentials",
            http::HeaderValue::from_static("true"),
        );
    }
}

/// Append a token to an existing `Vary` header (creating it if absent).
fn append_vary(headers: &mut HeaderMap, token: &str) {
    let existing = headers
        .get_all(http::header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    if existing.iter().any(|t| t.eq_ignore_ascii_case(token)) {
        return;
    }
    if existing.is_empty() {
        if let Ok(v) = http::HeaderValue::from_str(token) {
            headers.insert(http::header::VARY, v);
        }
    } else {
        let joined = format!("{}, {token}", existing.join(", "));
        headers.insert(
            http::header::VARY,
            http::HeaderValue::from_str(&joined)
                .unwrap_or_else(|_| http::HeaderValue::from_static("Origin")),
        );
    }
}

fn endpoint_authority(e: &Endpoint) -> String {
    if is_standard_port(&e.scheme, e.port) {
        e.host.clone()
    } else {
        format!("{}:{}", e.host, e.port)
    }
}

/// Case-insensitive, trailing-dot-tolerant host match: the (already
/// validated, bare) Target-Host value must equal the endpoint's bare hostname.
fn matches_endpoint(e: &Endpoint, target: &str) -> bool {
    normalize_alias(&e.host) == normalize_alias(target)
}

fn scope_key(
    cfg: &RateLimitConfig,
    sctx: &SecurityContext,
    parts: &axum::http::request::Parts,
    route: &Route,
) -> String {
    match cfg.scope {
        RateLimitScope::Global => "global".into(),
        RateLimitScope::Tenant => format!("tenant:{}", sctx.subject_tenant_id()),
        RateLimitScope::User => format!("user:{}", sctx.subject_id()),
        RateLimitScope::Ip => parts
            .extensions
            .get::<std::net::SocketAddr>()
            .map_or_else(|| "ip:unknown".into(), |a| format!("ip:{}", a.ip())),
        RateLimitScope::Route => format!("route:{}", route.id),
    }
}

/// Build the upstream request headers: hop-by-hop strip, Target-Host removal,
/// passthrough policy (applied to *inbound client* headers only), overlay of
/// plugin-authored headers, configured set/add/remove and Host replacement.
///
/// `client_headers` are the pristine inbound headers and are the only ones
/// subject to the passthrough filter.  `effective_headers` are the post-plugin
/// headers (auth credentials, guard/transform mutations).  Plugin-authored or
/// plugin-mutated headers always reach the upstream regardless of the
/// passthrough mode, because a bound plugin explicitly opted the gateway into
/// sending them (e.g. `x-api-key`).  Headers the client sent unchanged are
/// governed purely by the passthrough policy.  The upstream's configured
/// `set`/`add`/`remove` are applied last and always win.
pub fn build_upstream_headers(
    client_headers: &HeaderMap,
    effective_headers: &HeaderMap,
    upstream: &Upstream,
    authority: &str,
    passthrough: PassthroughMode,
) -> HeaderMap {
    // 1. Passthrough-filtered *inbound* headers.  Hop-by-hop headers, the
    //    Target-Host matrix header and any header nominated by the
    //    `Connection` value list (RFC 9110 §7.6.1) are dropped.
    let conn_named = connection_named_headers(effective_headers);
    let mut headers = HeaderMap::new();
    for (name, value) in client_headers {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str())
            || lname == TARGET_HOST_HEADER
            || conn_named.contains(&lname)
        {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }

    match passthrough {
        PassthroughMode::None => {
            let mut safe = HeaderMap::new();
            for (name, value) in &headers {
                if SAFE_PASSTHROUGH.contains(&name.as_str().to_ascii_lowercase().as_str()) {
                    safe.append(name.clone(), value.clone());
                }
            }
            headers = safe;
        }
        PassthroughMode::Allowlist => {
            let allow: Vec<String> = upstream
                .headers
                .request
                .passthrough_allowlist
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
            let mut kept = HeaderMap::new();
            for (name, value) in &headers {
                if allow.contains(&name.as_str().to_ascii_lowercase()) {
                    kept.append(name.clone(), value.clone());
                }
            }
            headers = kept;
        }
        PassthroughMode::All => {}
    }

    // 2. Overlay plugin-authored/mutated headers.  A header whose value the
    //    plugin pipeline changed (or introduced) is gateway-vouched and is
    //    always forwarded.  Unchanged inbound headers remain subject to the
    //    passthrough decision already made above.
    for (name, value) in effective_headers {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str())
            || lname == TARGET_HOST_HEADER
            || conn_named.contains(&lname)
        {
            continue;
        }
        let unchanged_inbound = client_headers
            .get_all(name)
            .iter()
            .any(|v| v.as_bytes() == value.as_bytes());
        if !unchanged_inbound {
            headers.insert(name.clone(), value.clone());
        }
    }

    for (name, value) in &upstream.headers.request.set {
        if let (Ok(n), Ok(v)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(n, v);
        }
    }
    for (name, value) in &upstream.headers.request.add {
        if let (Ok(n), Ok(v)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(n, v);
        }
    }
    for name in &upstream.headers.request.remove {
        if let Ok(n) = http::header::HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(n);
        }
    }
    if let Ok(v) = http::HeaderValue::from_str(authority) {
        headers.insert(http::header::HOST, v);
    }
    headers
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let conn_named = connection_named_headers(headers);
    let drop: Vec<http::header::HeaderName> = headers
        .keys()
        .filter(|name| {
            let lname = name.as_str().to_ascii_lowercase();
            HOP_BY_HOP.contains(&lname.as_str()) || conn_named.contains(&lname)
        })
        .cloned()
        .collect();
    for d in drop {
        headers.remove(d);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "unit tests assert on Result values"
    )]
    use super::*;
    use crate::domain::dto::{
        HeadersConfig, HttpMethod, MatchConfig, PluginsConfig, Protocol, ServerConfig,
    };
    use crate::domain::hierarchy::FlatTenantHierarchy;
    use crate::infra::plugin::build_registries;
    use crate::infra::storage::InMemoryRepository;
    use async_trait::async_trait;
    use http::Method;

    fn tenant() -> Uuid {
        Uuid::nil()
    }

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: "http".into(),
            host: host.into(),
            port,
        }
    }

    fn tls_endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: "https".into(),
            host: host.into(),
            port,
        }
    }

    fn upstream(alias: &str, eps: Vec<Endpoint>) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant(),
            enabled: true,
            alias: alias.into(),
            tags: vec![],
            server: ServerConfig { endpoints: eps },
            protocol: Protocol::Http,
            auth: AuthConfig::default(),
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
            created_at: 0,
        }
    }

    fn route(upstream_id: Uuid, path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant(),
            enabled: true,
            upstream_id,
            priority: 0,
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: path.into(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            tags: vec![],
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
            created_at: 0,
        }
    }

    fn svc(repo: Arc<dyn OagwRepository>) -> DataPlaneService {
        let config = OagwConfig::default();
        let (auth, guard, transform) = build_registries(&config, None);
        DataPlaneService::new(
            repo,
            Arc::new(FlatTenantHierarchy),
            auth,
            guard,
            transform,
            Arc::new(TestEngine),
            &config,
        )
    }

    struct TestEngine;
    #[async_trait::async_trait]
    impl ProxyEngine for TestEngine {
        async fn call(
            &self,
            _req: ProxyRequest,
        ) -> Result<crate::infra::proxy::ProxyResponse, ProxyError> {
            Ok(crate::infra::proxy::ProxyResponse {
                status: http::StatusCode::OK,
                headers: HeaderMap::new(),
                body: crate::infra::proxy::boxed_bytes_body("ok"),
            })
        }
        async fn upgrade(&self, _req: ProxyRequest) -> Result<UpgradeOutcome, ProxyError> {
            Ok(UpgradeOutcome::Response(
                crate::infra::proxy::ProxyResponse {
                    status: http::StatusCode::OK,
                    headers: HeaderMap::new(),
                    body: crate::infra::proxy::boxed_bytes_body("ok"),
                },
            ))
        }
    }

    #[test]
    fn select_endpoint_single_accepts_matching_target() {
        let up = upstream("s", vec![endpoint("a.example.com", 8080)]);
        let mut headers = HeaderMap::new();
        // Bare hostname (no port) per ADR-0007 strict Target-Host validation.
        headers.insert(TARGET_HOST_HEADER, "a.example.com".parse().unwrap());
        let got = svc(Arc::new(InMemoryRepository::new()))
            .select_endpoint(&up, &headers)
            .unwrap();
        assert_eq!(got.host, "a.example.com");
    }

    #[test]
    fn select_endpoint_matches_target_case_and_trailing_dot_insensitively() {
        let up = upstream("s", vec![endpoint("api.example.com", 443)]);
        let svc = svc(Arc::new(InMemoryRepository::new()));
        for target in ["API.Example.COM", "api.example.com.", "Api.Example.com"] {
            let mut headers = HeaderMap::new();
            headers.insert(TARGET_HOST_HEADER, target.parse().unwrap());
            let got = svc.select_endpoint(&up, &headers).unwrap();
            assert_eq!(got.host, "api.example.com");
        }
    }

    #[test]
    fn select_endpoint_single_rejects_wrong_target() {
        let up = upstream("s", vec![endpoint("a.example.com", 8080)]);
        let mut headers = HeaderMap::new();
        headers.insert(TARGET_HOST_HEADER, "b.example.com".parse().unwrap());
        let err = svc(Arc::new(InMemoryRepository::new()))
            .select_endpoint(&up, &headers)
            .unwrap_err();
        // Well-formed but non-matching → unknown_target_host (not invalid).
        assert!(matches!(err, DomainError::UnknownTargetHost { .. }));
    }

    #[test]
    fn select_endpoint_rejects_malformed_target_before_matching() {
        // ADR-0007: no port, path or special characters in Target-Host.
        let up = upstream("s", vec![endpoint("a.example.com", 8080)]);
        let svc = svc(Arc::new(InMemoryRepository::new()));
        for bad in [
            "a.example.com:8080",
            "a.example.com/path",
            "bad_host",
            "a b.c",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(TARGET_HOST_HEADER, bad.parse().unwrap());
            let err = svc.select_endpoint(&up, &headers).unwrap_err();
            assert!(
                matches!(err, DomainError::InvalidTargetHost(ref v) if v == bad),
                "expected InvalidTargetHost for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn select_endpoint_multi_explicit_round_robins() {
        let up = upstream(
            "pool",
            vec![
                endpoint("a.example.com", 443),
                endpoint("b.example.com", 443),
            ],
        );
        let svc = svc(Arc::new(InMemoryRepository::new()));
        let e1 = svc.select_endpoint(&up, &HeaderMap::new()).unwrap();
        let e2 = svc.select_endpoint(&up, &HeaderMap::new()).unwrap();
        assert_ne!(e1.host, e2.host);
    }

    #[test]
    fn select_endpoint_multi_derived_requires_target_host() {
        let svc = svc(Arc::new(InMemoryRepository::new()));
        // alias == derived (vendor.com, standard https:443) ⇒ Target-Host
        // REQUIRED to disambiguate the pool.
        let up = upstream(
            "vendor.com",
            vec![
                tls_endpoint("us.vendor.com", 443),
                tls_endpoint("eu.vendor.com", 443),
            ],
        );
        let err = svc.select_endpoint(&up, &HeaderMap::new()).unwrap_err();
        assert!(matches!(err, DomainError::MissingTargetHost { .. }));
    }

    #[test]
    fn select_endpoint_multi_derived_resolves_target() {
        let svc = svc(Arc::new(InMemoryRepository::new()));
        let up = upstream(
            "vendor.com",
            vec![
                tls_endpoint("us.vendor.com", 443),
                tls_endpoint("eu.vendor.com", 443),
            ],
        );
        let mut headers = HeaderMap::new();
        headers.insert(TARGET_HOST_HEADER, "eu.vendor.com".parse().unwrap());
        let got = svc.select_endpoint(&up, &headers).unwrap();
        assert_eq!(got.host, "eu.vendor.com");
    }

    #[test]
    fn build_upstream_headers_strips_hop_by_hop_and_target_host() {
        let mut client = HeaderMap::new();
        client.insert(http::header::HOST, "client.example".parse().unwrap());
        client.insert(TARGET_HOST_HEADER, "a.example.com".parse().unwrap());
        client.insert(http::header::CONNECTION, "keep-alive".parse().unwrap());
        client.insert("x-custom", "keep".parse().unwrap());
        let up = upstream("s", vec![endpoint("a.example.com", 80)]);
        let built = build_upstream_headers(
            &client,
            &client,
            &up,
            "a.example.com",
            PassthroughMode::None,
        );
        assert!(built.get(http::header::HOST).is_some());
        assert_eq!(built.get(http::header::HOST).unwrap(), "a.example.com");
        assert!(built.get(TARGET_HOST_HEADER).is_none());
        assert!(built.get(http::header::CONNECTION).is_none());
        // passthrough None drops non-safe inbound headers
        assert!(built.get("x-custom").is_none());
    }

    #[test]
    fn build_upstream_headers_overlays_plugin_headers() {
        let mut client = HeaderMap::new();
        client.insert("x-api-key", "client-sent".parse().unwrap());
        client.insert("x-custom", "keep".parse().unwrap());
        // Plugin pipeline replaced x-api-key with the credential and added a
        // fresh x-request-id.
        let mut effective = client.clone();
        effective.insert("x-api-key", "sk-secret".parse().unwrap());
        effective.insert("x-request-id", "req_new".parse().unwrap());
        let up = upstream("s", vec![endpoint("a.example.com", 80)]);
        let built = build_upstream_headers(
            &client,
            &effective,
            &up,
            "a.example.com",
            PassthroughMode::None,
        );
        // Plugin-authored/mutated headers always forwarded...
        assert_eq!(built.get("x-api-key").unwrap(), "sk-secret");
        assert_eq!(built.get("x-request-id").unwrap(), "req_new");
        // ...while unchanged non-safe inbound headers stay filtered.
        assert!(built.get("x-custom").is_none());
    }

    #[test]
    fn build_upstream_headers_strips_connection_named_headers() {
        // RFC 9110 §7.6.1: `Connection: keep-alive, X-Strip-Me` also strips
        // the `x-strip-me` (and `keep-alive`) headers, not just `Connection`.
        let mut client = HeaderMap::new();
        client.insert(
            http::header::CONNECTION,
            "keep-alive, X-Strip-Me".parse().unwrap(),
        );
        client.insert("x-strip-me", "value".parse().unwrap());
        client.insert("keep-alive", "timeout=5".parse().unwrap());
        client.insert("x-keep", "stay".parse().unwrap());
        let up = upstream("s", vec![endpoint("a.example.com", 80)]);
        let built =
            build_upstream_headers(&client, &client, &up, "a.example.com", PassthroughMode::All);
        assert!(built.get(http::header::CONNECTION).is_none());
        assert!(built.get("x-strip-me").is_none());
        assert!(built.get("keep-alive").is_none());
        assert!(built.get("x-keep").is_some());
    }

    #[test]
    fn strip_hop_by_hop_removes_connection_named_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONNECTION,
            "X-Foo, keep-alive".parse().unwrap(),
        );
        headers.insert("x-foo", "1".parse().unwrap());
        headers.insert("x-bar", "2".parse().unwrap());
        strip_hop_by_hop(&mut headers);
        assert!(headers.get("connection").is_none());
        assert!(headers.get("x-foo").is_none());
        assert!(headers.get("x-bar").is_some());
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn response_json(resp: Response) -> serde_json::Value {
        serde_json::from_slice(
            &rt()
                .block_on(BodyExt::collect(resp.into_body()))
                .unwrap()
                .to_bytes(),
        )
        .unwrap()
    }

    fn svc_with_cap(cap: usize) -> DataPlaneService {
        let config = OagwConfig {
            allow_http_upstream: true,
            max_request_body_bytes: cap as u64,
            ..Default::default()
        };
        let (auth, guard, transform) = build_registries(&config, None);
        DataPlaneService::new(
            Arc::new(InMemoryRepository::new()),
            Arc::new(FlatTenantHierarchy),
            auth,
            guard,
            transform,
            Arc::new(TestEngine),
            &config,
        )
    }

    #[test]
    fn buffer_body_rejects_invalid_content_length() {
        let svc = svc_with_cap(1024);
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "abc".parse().unwrap());
        let err = rt()
            .block_on(svc.buffer_body(&headers, Body::from(b"x".to_vec())))
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn buffer_body_rejects_content_length_over_cap_before_reading() {
        // Declared CL over the cap → 413 immediately (before body reading).
        let svc = svc_with_cap(4);
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "100".parse().unwrap());
        let err = rt()
            .block_on(svc.buffer_body(&headers, Body::from(b"tiny".to_vec())))
            .unwrap_err();
        assert!(matches!(err, DomainError::PayloadTooLarge(_)));
    }

    #[test]
    fn buffer_body_rejects_non_chunked_transfer_encoding() {
        let svc = svc_with_cap(1024);
        let mut headers = HeaderMap::new();
        headers.insert(http::header::TRANSFER_ENCODING, "gzip".parse().unwrap());
        let err = rt()
            .block_on(svc.buffer_body(&headers, Body::from(b"x".to_vec())))
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn buffer_body_rejects_declared_length_mismatch_after_read() {
        let svc = svc_with_cap(1024);
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "5".parse().unwrap());
        let err = rt()
            .block_on(svc.buffer_body(&headers, Body::from(b"hi".to_vec())))
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn buffer_body_accepts_chunked_and_well_formed() {
        let svc = svc_with_cap(1024);
        let mut headers = HeaderMap::new();
        headers.insert(http::header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        let out = rt()
            .block_on(svc.buffer_body(&headers, Body::from(b"hello".to_vec())))
            .unwrap();
        assert_eq!(out, b"hello");
    }

    /// Probe transform plugin whose `transform_error` hook rewrites the
    /// problem `type`/status to prove the hook is invoked on gateway errors.
    struct ProbeErrorTransform {
        rewritten_type: String,
    }

    #[async_trait]
    impl crate::domain::plugin::TransformPlugin for ProbeErrorTransform {
        fn id(&self) -> &'static str {
            "probe-error"
        }
        fn plugin_type(&self) -> &'static str {
            "probe-error"
        }
        async fn transform_request(&self, _ctx: &mut RequestContext) -> Result<(), PluginError> {
            Ok(())
        }
        async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), PluginError> {
            Ok(())
        }
        async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError> {
            // Receives the ORIGINAL error type/status before rendering.
            assert_eq!(ctx.status, 403, "hook must see the domain error's status");
            ctx.error_type = self.rewritten_type.clone();
            ctx.status = 418;
            Ok(())
        }
    }

    #[test]
    fn gateway_error_invokes_transform_error_hooks() {
        let config = OagwConfig::default();
        let mut transforms = crate::domain::plugin::TransformPluginRegistry::new();
        transforms.register(std::sync::Arc::new(ProbeErrorTransform {
            rewritten_type: "gts.cf.core.errors.err.v1~cf.oagw.rewritten.v1".into(),
        }));
        let svc = DataPlaneService::new(
            Arc::new(InMemoryRepository::new()),
            Arc::new(FlatTenantHierarchy),
            crate::domain::plugin::AuthPluginRegistry::new(),
            crate::domain::plugin::GuardPluginRegistry::new(),
            transforms,
            Arc::new(TestEngine),
            &config,
        );
        let e = DomainError::PermissionDenied("proxy".into());
        let binding = PluginBinding {
            plugin_ref: "probe-error".into(),
            config: serde_json::Value::Null,
        };
        let resp = rt().block_on(svc.gateway_error(e, &[binding]));
        assert_eq!(resp.status(), 418);
        let body = response_json(resp);
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.rewritten.v1"
        );
        assert_eq!(body["status"], 418);
    }

    #[test]
    fn gateway_error_extension_fields_are_snake_case() {
        let svc = svc(Arc::new(InMemoryRepository::new()));
        // UnknownTargetHost carries invalid_value + valid_hosts (ADR-0007).
        let e = DomainError::UnknownTargetHost {
            invalid_value: "apac.vendor.com".into(),
            valid_hosts: vec!["us.vendor.com".into(), "eu.vendor.com".into()],
        };
        let resp = rt().block_on(svc.gateway_error(e, &[]));
        let body = response_json(resp);
        assert_eq!(body["invalid_value"], "apac.vendor.com");
        assert_eq!(
            body["valid_hosts"],
            serde_json::json!(["us.vendor.com", "eu.vendor.com"])
        );
        assert!(body.get("invalidValue").is_none());
        assert!(body.get("validHosts").is_none());

        // InvalidTargetHost carries invalid_value.
        let e = DomainError::InvalidTargetHost("us.vendor.com:8443".into());
        let resp = rt().block_on(svc.gateway_error(e, &[]));
        let body = response_json(resp);
        assert_eq!(body["invalid_value"], "us.vendor.com:8443");
    }

    #[test]
    fn best_route_uses_method_then_longest_path_then_priority() {
        let up_id = Uuid::new_v4();
        let mk = |methods: Vec<HttpMethod>, path: &str, priority: u32, enabled: bool| {
            let mut r = route(up_id, path);
            r.enabled = enabled;
            r.priority = priority;
            if let Some(h) = &mut r.match_config.http {
                h.methods = methods;
            }
            r
        };
        let routes = vec![
            mk(vec![HttpMethod::Get], "/v1", 0, true),
            mk(vec![HttpMethod::Post], "/v1/users", 0, true),
            mk(vec![HttpMethod::Get], "/v1/users", 5, true),
            mk(vec![HttpMethod::Get], "/v1/users", 9, false), // disabled ignored
        ];
        // GET /v1/users → longest path /v1/users; ties broken by priority.
        let best = DataPlaneService::best_route(&routes, up_id, &Method::GET, "/v1/users").unwrap();
        assert_eq!(best.match_config.http.as_ref().unwrap().path, "/v1/users");
        assert_eq!(best.priority, 5);
        // POST only matches the POST route despite the longer disabled one.
        let best_post =
            DataPlaneService::best_route(&routes, up_id, &Method::POST, "/v1/users").unwrap();
        assert_eq!(best_post.priority, 0);
        assert_eq!(
            best_post.match_config.http.as_ref().unwrap().path.len(),
            "/v1/users".len()
        );
        // Method with no matching route → None.
        assert!(
            DataPlaneService::best_route(&routes, up_id, &Method::DELETE, "/v1/users").is_none()
        );
        // Path outside every route's prefix → None even when a method admits
        // the request (path-aware selection).
        assert!(DataPlaneService::best_route(&routes, up_id, &Method::GET, "/other").is_none());
    }

    #[test]
    fn best_route_selects_by_request_path_not_longest_overall() {
        // Regression (deliverable verification): with multiple routes of the
        // same method on one upstream, the pre-selected route must be the one
        // whose path prefix-matches the request — not merely the longest path
        // in the set (which used to 404 every shorter-path request).
        let up_id = Uuid::new_v4();
        let mk = |methods: Vec<HttpMethod>, path: &str| {
            let mut r = route(up_id, path);
            if let Some(h) = &mut r.match_config.http {
                h.methods = methods;
            }
            r
        };
        let routes = vec![
            mk(vec![HttpMethod::Get], "/hello"),
            mk(vec![HttpMethod::Get], "/echo-headers"),
            mk(vec![HttpMethod::Get], "/sse"),
            mk(vec![HttpMethod::Get], "/status-503"),
        ];
        for (path, want) in [
            ("/hello", "/hello"),
            ("/echo-headers", "/echo-headers"),
            ("/sse", "/sse"),
            ("/status-503", "/status-503"),
            ("/hello/sub", "/hello"), // prefix still matches the shorter base
        ] {
            let best = DataPlaneService::best_route(&routes, up_id, &Method::GET, path).unwrap();
            assert_eq!(
                best.match_config.http.as_ref().unwrap().path,
                want,
                "request '{path}' must select route '{want}'"
            );
        }
        // A request under /hello regardless of /echo-headers being the set's
        // longest path.
        assert_eq!(
            DataPlaneService::best_route(&routes, up_id, &Method::GET, "/hello")
                .unwrap()
                .match_config
                .http
                .as_ref()
                .unwrap()
                .path,
            "/hello"
        );
    }

    #[test]
    fn match_route_enforces_path_method_and_suffix_mode() {
        let up = upstream("s", vec![endpoint("a.example.com", 80)]);
        let mut r = route(Uuid::new_v4(), "/v1");
        r.match_config.http.iter_mut().for_each(|h| {
            h.methods = vec![HttpMethod::Get];
            h.path = "/v1".into();
        });
        let resolved = Resolved {
            chain: vec![up.clone()],
            route: r.clone(),
        };

        // Matching path + method → Ok.
        DataPlaneService::match_route(&resolved, "/v1/users", &Method::GET)
            .expect("matching request should resolve");
        // Path outside the base → 404.
        assert!(matches!(
            DataPlaneService::match_route(&resolved, "/other", &Method::GET).unwrap_err(),
            DomainError::RouteNotFound { .. }
        ));
        // Method not allowed → 404.
        assert!(matches!(
            DataPlaneService::match_route(&resolved, "/v1", &Method::POST).unwrap_err(),
            DomainError::RouteNotFound { .. }
        ));
        // Suffix with path_suffix_mode=disabled → 400 Validation.
        r.match_config.http.iter_mut().for_each(|h| {
            h.path_suffix_mode = PathSuffixMode::Disabled;
        });
        let resolved_disabled = Resolved {
            chain: vec![up],
            route: r,
        };
        assert!(matches!(
            DataPlaneService::match_route(&resolved_disabled, "/v1/x", &Method::GET).unwrap_err(),
            DomainError::Validation(_)
        ));
    }
}
