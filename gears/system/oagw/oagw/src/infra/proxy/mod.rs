//! Data-plane proxy: the request pipeline gate plus the pingora bridge.
//!
//! Implements `cpt-cf-oagw-feature-data-plane`:
//!
//! - [`DataPlaneGate`] — the request pipeline (`cpt-cf-oagw-algo-data-plane-
//!   pipeline-execution`): DP L1 resolution, auth plugins (credential
//!   resolution and injection), token-bucket rate limiting, the PEP gate,
//!   CORS (ADR 0004), the SSRF guard, guard plugins, and the request-id
//!   transform. Returns either a [`GatePass`] (forward) or an RFC 9457
//!   [`ProblemResponse`] carrying `X-OAGW-Error-Source: gateway`.
//! - [`ProxyBridgeHandle`] — spawns the pingora `Server` (bound to
//!   `127.0.0.1:0`) that hosts [`DataPlaneService`]. The axum proxy surface
//!   forwards into this internal listener through [`relay_request`] (the
//!   hand-rolled HTTP/1.1 relay in [`relay`]). Each spawn mints a random
//!   relay secret that the surface must stamp on every internal request and
//!   the gate verifies (so a loopback port probe cannot spoof the caller
//!   identity), and [`ProxyBridgeHandle::stop`] actually shuts the pingora
//!   server down (no ghost listener).
//!
//! Internal header contract between the axum relay and the pingora gate
//! (all `x-oagw-*` headers are stripped before forwarding upstream):
//!
//! - `x-oagw-backend-alias` — route alias to proxy.
//! - `x-oagw-tenant-id` / `x-oagw-subject-id` — caller identity.
//! - `x-oagw-bearer` / `x-oagw-token-scopes` — caller token for the PEP.
//! - `x-oagw-relay-secret` — per-bridge bearer credential stamped by the
//!   relay and verified by [`DataPlaneService`] before any identity header
//!   is trusted.
//! - `x-oagw-internal-client-secret` — set by [`DataPlaneGate`] from the
//!   effective `oauth2_*` plugin config so the plugin can resolve the client
//!   secret from the credential store.

// DoD traceability (`cpt-cf-oagw-dod-data-plane-*` — to_code markers).
// @cpt-dod:cpt-cf-oagw-dod-data-plane-proxy-service:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-l1-config-cache:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-plugin-registry:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-pep-gate:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-rate-limiter:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-ssrf-guard:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-cors:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-credential-injection:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-error-source:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-streaming:p2
// @cpt-dod:cpt-cf-oagw-dod-data-plane-test-harness:p2
pub mod proxy_http;
pub mod relay;

use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use tracing::{error, info};

use async_trait::async_trait;
use pingora_core::server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use pingora_proxy::http_proxy_service;

use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;

use crate::domain::cors::{CorsDecision, CorsGate};
use crate::domain::credentials::CredentialResolver;
use crate::domain::error::DomainError;
use crate::domain::models::{EffectiveRouteConfig, PluginKind, RouteHttpMatch, UpstreamScheme};
use crate::domain::plugins::{AuthContext, PluginRegistry};
use crate::domain::rate_limit::{BucketOutcome, RateLimiterRegistry};
use crate::domain::repository::ControlPlaneService;

use crate::domain::ssrf::SsrfGuard;

pub use proxy_http::DataPlaneService;
pub use relay::relay_request;

/// Internal header carrying the route alias from the ingress relay.
pub const HDR_BACKEND_ALIAS: &str = "x-oagw-backend-alias";
/// Internal header carrying the caller tenant id.
pub const HDR_TENANT_ID: &str = "x-oagw-tenant-id";
/// Internal header carrying the caller subject id.
pub const HDR_SUBJECT_ID: &str = "x-oagw-subject-id";
/// Internal header carrying the caller bearer token (for PDP forwarding).
pub const HDR_BEARER: &str = "x-oagw-bearer";
/// Internal header carrying the caller token scopes (comma-separated).
pub const HDR_TOKEN_SCOPES: &str = "x-oagw-token-scopes";
/// Internal header set by the gate from the effective oauth2 plugin config.
pub const HDR_INTERNAL_CLIENT_SECRET: &str = "x-oagw-internal-client-secret";
/// Internal header carrying the per-bridge relay secret; the pingora gate
/// rejects internal requests that do not present it.
pub const HDR_RELAY_SECRET: &str = "x-oagw-relay-secret";
/// Authoritative error-source distinction header (ADR 0007).
pub const HDR_ERROR_SOURCE: &str = "X-OAGW-Error-Source";

/// GTS error-type identifiers from the authoritative proxy error catalog
/// (DESIGN.md §3.3).
pub mod error_types {
    pub const VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    pub const AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    pub const RATE_LIMIT: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    pub const PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    pub const DOWNSTREAM: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    pub const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    pub const TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    pub const INTERNAL: &str = "gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1";
}

/// Outcome of a successful gate pass: the request may be forwarded.
pub struct GatePass {
    /// Effective configuration resolved from the DP L1 cache.
    pub effective: Arc<EffectiveRouteConfig>,
    /// Executable plugin registry (auth/guard/transform) for this request.
    pub registry: PluginRegistry,
    /// Request id the transform generated (or `None`).
    pub request_id: Option<String>,
    /// Upstream credential/identity headers to inject before forwarding.
    pub auth_injections: Vec<(String, String)>,
    /// Headers to attach to the proxied response (CORS, request id echo).
    pub response_headers: Vec<(String, String)>,
    /// SSRF-validated upstream address the transport must connect to instead
    /// of re-resolving the hostname (DNS-rebind hardening); `None` when the
    /// guard is disabled.
    pub pinned_ip: Option<IpAddr>,
}

/// An RFC 9457 problem response authored by the gate (or the transport).
#[derive(Debug, Clone)]
pub struct ProblemResponse {
    /// HTTP status code.
    pub status: u16,
    /// GTS error-type identifier (`type`).
    pub error_type: &'static str,
    /// Short human-readable title.
    pub title: String,
    /// Detailed diagnostic message.
    pub detail: String,
    /// `gateway` or `upstream`.
    pub error_source: &'static str,
    /// Extra response headers (e.g. `Retry-After`, `X-RateLimit-*`, CORS).
    pub extra_headers: Vec<(String, String)>,
}

impl ProblemResponse {
    /// Builds an RFC 9457 problem-response from its pieces.
    #[must_use]
    pub fn new(
        status: u16,
        error_type: &'static str,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            error_type,
            title: title.into(),
            detail: detail.into(),
            error_source: "gateway",
            extra_headers: Vec::new(),
        }
    }

    /// Serializes the problem+json body.
    #[must_use]
    pub fn to_body(&self) -> serde_json::Value {
        serde_json::json!({
            "type": self.error_type,
            "title": self.title,
            "status": self.status,
            "detail": self.detail,
        })
    }
}

/// The data-plane request pipeline gate (`cpt-cf-oagw-algo-data-plane-pipeline-execution`).
///
/// Owns the shared state the pipeline needs and evaluates one proxied request
/// end to end — from DP-L1 resolution through guard plugins — returning a
/// [`GatePass`] (forward) or a gateway-sourced [`ProblemResponse`].
pub struct DataPlaneGate {
    control: Arc<ControlPlaneService>,
    enforcer: Arc<authz_resolver_sdk::pep::PolicyEnforcer>,
    resolver: Arc<CredentialResolver>,
    rate_limiter: Arc<RateLimiterRegistry>,
    allow_http_upstream: bool,
    ssrf: SsrfGuard,
    fallback_timeout: Duration,
    resource: authz_resolver_sdk::pep::ResourceType,
}

/// PEP action for invoking the proxy surface.
pub const PROXY_ACTION: &str = "invoke";

impl DataPlaneGate {
    /// Builds the gate.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        control: Arc<ControlPlaneService>,
        enforcer: Arc<authz_resolver_sdk::pep::PolicyEnforcer>,
        resolver: Arc<CredentialResolver>,
        rate_limiter: Arc<RateLimiterRegistry>,
        allow_http_upstream: bool,
        ssrf_enabled: bool,
        fallback_timeout: Duration,
    ) -> Self {
        let resource = authz_resolver_sdk::pep::ResourceType::from_static(
            gts_id!("cf.core.oagw.proxy.v1~"),
            &[
                toolkit_security::pep_properties::OWNER_TENANT_ID,
                toolkit_security::pep_properties::RESOURCE_ID,
            ],
        );
        // Prune rate-limit buckets when a route is deleted from the control
        // plane (no unbounded per-route bucket growth).
        control.on_route_delete({
            let rate_limiter = rate_limiter.clone();
            move |alias| rate_limiter.prune(alias)
        });
        Self {
            control,
            enforcer,
            resolver,
            rate_limiter,
            allow_http_upstream,
            ssrf: SsrfGuard::new(ssrf_enabled),
            fallback_timeout,
            resource,
        }
    }

    /// Accessor for the backing control plane (prune-hook wiring).
    #[must_use]
    pub fn control(&self) -> &Arc<ControlPlaneService> {
        &self.control
    }

    /// Evaluates one proxied request.
    ///
    /// The request's headers are mutated only to add the internal
    /// `x-oagw-internal-client-secret` for the oauth2 plugins (stripped
    /// before forwarding). Authentication failures, rate-limit
    /// exhaustions, PEP denials, CORS/SSRF/guard rejections return an
    /// authoritative gateway problem response.
    pub async fn evaluate(
        &self,
        alias: &str,
        method: &str,
        path: &str,
        security: &SecurityContext,
        request_headers: &mut HeaderMap,
    ) -> Result<GatePass, ProblemResponse> {
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-load-effective
        // DP L1 resolution (1000-entry cache, refreshed on CP invalidation).
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-alias-unknown
        let effective = self
            // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-alias-unknown
            .control
            .l1_cache()
            .resolve(alias, &self.control, self.fallback_timeout)
            .ok_or_else(|| {
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-404
                ProblemResponse::new(
                    // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-404
                    404,
                    error_types::ROUTE_NOT_FOUND,
                    "Route not found",
                    format!("proxy alias `{alias}` does not resolve to a route"),
                )
            })?;
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-load-effective
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-alias-known
        // (alias resolved to a known route from the DP L1 cache)
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-alias-known
        // Route predicates (`methods` / `http_matches`) are enforced before
        // authz or any upstream touch: a method not in the declared set emits
        // 405 with the canonical `Allow` header; a non-matching http-match set
        // means the route does not apply to this request (404).
        if let Some(methods) = &effective.methods
            && !methods.iter().any(|m| m.eq_ignore_ascii_case(method))
        {
            let mut resp = ProblemResponse::new(
                405,
                error_types::VALIDATION,
                "Method not allowed",
                format!("route `{alias}` does not allow method `{method}`"),
            );
            resp.extra_headers
                .push(("Allow".to_owned(), methods.join(", ")));
            return Err(resp);
        }
        if !effective.http_matches.is_empty()
            && !effective
                .http_matches
                .iter()
                .any(|rule| http_match_applies(rule, method, path))
        {
            return Err(ProblemResponse::new(
                404,
                error_types::ROUTE_NOT_FOUND,
                "Route not found",
                format!("request path `{path}` does not match any http_match of route `{alias}`"),
            ));
        }
        // Tenant scope: the caller was authn-resolved to a tenant by the
        // host (`subject_tenant_id`); the tenant-resolver dependency is wired
        // and effective configuration is tenant-scoped through the CP.
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-tenant-resolve
        // (effective configuration is tenant-scoped through the CP)
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-tenant-resolve
        // Inject the oauth2 client-secret reference the plugins consume
        // without ever exposing secrets to the caller.
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-credential-resolve
        for plugin in &effective.plugins {
            // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-credential-resolve
            if matches!(
                plugin.kind,
                PluginKind::OAuth2ClientCred | PluginKind::OAuth2ClientCredBasic
            ) && plugin.enabled
                && let Some(cred_ref) = plugin
                    .config
                    .get("cred_ref")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
            {
                let value = http::HeaderValue::from_str(cred_ref).map_err(|_| {
                    ProblemResponse::new(
                        500,
                        error_types::INTERNAL,
                        "Internal",
                        "internal client-secret reference is not a valid header value",
                    )
                })?;
                request_headers.insert(HDR_INTERNAL_CLIENT_SECRET, value);
            }
        }

        // Auth plugins (credential resolution + injection happen inside the
        // plugins via the resolver).
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-auth-plugins
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-loop
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-auth-plugins
        let registry = PluginRegistry::try_build(&effective.plugins, self.resolver.clone())
            .map_err(|e| err_from_domain(400, e, "plugin configuration is invalid"))?;
        let ctx = AuthContext {
            headers: request_headers,
            security,
            tenant_id: security.subject_tenant_id().to_string(),
        };
        let auth_injections = match registry.run_auth(&ctx).await {
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-plugin-exec
            Ok(inj) => {
                // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-plugin-exec
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-done
                inj
                // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-done
            }
            Err(e) => {
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-auth-fail
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-401
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-missing
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-401
                return Err(err_from_domain(401, e, "authentication failed"));
                // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-auth-fail
                // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-401
                // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-missing
                // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-401
            }
        };
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-auth-loop
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-auth-ok
        // (auth plugins accepted the request credentials)
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-auth-ok
        // Rate limit (stricter-wins from the effective config).
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-rate-limit
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-rapid-request
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-check-bucket
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-rate-limit
        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-rapid-request
        let rate_key = format!("{}|{}", alias, security.subject_id());
        if let Some(limit) = &effective.rate_limit {
            match self
                .rate_limiter
                .admit(&rate_key, limit.capacity, limit.refill_per_sec)
            {
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-rate-ok
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-admitted
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-consume-token
                BucketOutcome::Admitted { .. } => {}
                // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-rate-ok
                // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-admitted
                // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-consume-token
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-rate-limit-hit
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-429
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-bucket-exhausted
                BucketOutcome::Rejected { retry_after_secs } => {
                    // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-rate-limit-hit
                    // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-429
                    // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-bucket-exhausted
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-bucket-check
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-exhausted
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-try
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-catch
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-fallback
                    let mut resp = ProblemResponse::new(
                        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-bucket-check
                        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-exhausted
                        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-try
                        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-catch
                        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-fallback
                        429,
                        error_types::RATE_LIMIT,
                        "Too many requests",
                        "token bucket exhausted",
                    );
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-ok
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-headers
                    resp.extra_headers.push((
                        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-ok
                        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-headers
                        "Retry-After".to_owned(),
                        retry_after_secs.to_string(),
                    ));
                    resp.extra_headers
                        .push(("X-RateLimit-Limit".to_owned(), limit.capacity.to_string()));
                    resp.extra_headers
                        .push(("X-RateLimit-Remaining".to_owned(), "0".to_owned()));
                    resp.extra_headers
                        .push(("X-RateLimit-Reset".to_owned(), retry_after_secs.to_string()));
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-source
                    // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-return
                    // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-bucket-429
                    return Err(resp);
                    // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-source
                    // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-429-return
                    // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-bucket-429
                }
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-check-bucket
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-to-main-flow
        // (rate-limited sub-flow returns to the main proxy flow)
        // @cpt-end:cpt-cf-oagw-flow-data-plane-rate-limited-request:ph-1:inst-to-main-flow
        // PEP gate: `gts.cf.core.oagw.proxy.v1~:invoke`.
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-request
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep
        match self
            .enforcer
            .access_scope(security, &self.resource, PROXY_ACTION, None)
            .await
        {
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep-allow
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-passed
            Ok(_) => {}
            // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep-allow
            // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-passed
            Err(e) => {
                // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep-deny
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-denied
                // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-deny
                return Err(pep_error(e));
                // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep-deny
                // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-denied
                // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-deny
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-pep-request

        // CORS per ADR 0004.
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-run-cors
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors
        let cors_gate = CorsGate::new(Some(effective.cors.clone()));
        let origin = request_headers.get("origin").and_then(|v| v.to_str().ok());
        let mut response_headers = Vec::new();
        match cors_gate.evaluate(method, origin) {
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors-ok
            CorsDecision::Passthrough => {}
            // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors-ok
            CorsDecision::Allowed { headers } => {
                response_headers.extend(headers);
            }
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors-fail
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors-error
            CorsDecision::OriginDenied => {
                // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors-fail
                // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-cors-error
                let mut resp = ProblemResponse::new(
                    403,
                    error_types::AUTH_FAILED,
                    "Forbidden",
                    "CORS: origin not allowed",
                );
                resp.extra_headers
                    .push(("Vary".to_owned(), "Origin".to_owned()));
                return Err(resp);
            }
            CorsDecision::Preflight { headers } => {
                // Preflight answered locally (204).
                let mut resp =
                    ProblemResponse::new(204, error_types::VALIDATION, "Preflight", "OK");
                resp.extra_headers.extend(headers);
                return Err(resp);
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-run-cors

        // SSRF guard.
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-run-ssrf
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf
        let upstream = effective.upstream.as_ref().ok_or_else(|| {
            ProblemResponse::new(
                503,
                error_types::LINK_UNAVAILABLE,
                "Upstream unavailable",
                "effective route has no upstream",
            )
        })?;
        if upstream.scheme == UpstreamScheme::Http && !self.allow_http_upstream {
            // HTTPS-only by default (SSRF policy); http is an opt-in.
            return Err(ProblemResponse::new(
                403,
                error_types::VALIDATION,
                "Transport not permitted",
                format!(
                    "plaintext http upstream `{}` not permitted (allow_http_upstream=false)",
                    upstream.host
                ),
            ));
        }
        // `check_host` returns the validated address set so the transport can
        // pin the upstream connection (no post-validation re-resolution).
        let pinned = match self.ssrf.check_host(&upstream.host) {
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf-block
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf-block-return
            Err(e) => {
                return Err(ProblemResponse::new(
                    // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf-block
                    // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf-block-return
                    403,
                    error_types::VALIDATION,
                    "SSRF blocked",
                    e.to_string(),
                ));
            }
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf-ok
            Ok(validated) => validated,
            // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-ssrf-ok
        };
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-run-ssrf
        let pinned_ip = pinned.into_iter().next();

        // Guard plugins + request-id transform.
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guards
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-run-guards
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guards
        let request_id = registry.apply_request_id(request_headers);
        let guard_ctx = AuthContext {
            headers: request_headers,
            security,
            tenant_id: security.subject_tenant_id().to_string(),
        };
        if let Err(e) = registry.run_guards(&guard_ctx).await {
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guard-reject
            // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guard-return
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-guard-blocked
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-guard-block
            return Err(err_from_domain(400, e, "request rejected by guard"));
            // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guard-reject
            // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guard-return
            // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-guard-blocked
            // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-guard-block
        }
        // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guard-ok
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-guards-passed
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-run-guards
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-guard-ok
        // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-guards-passed

        // Resolved upstream credentials ride along for injection at the
        // transport stage (upstream_request_filter).
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-inject
        Ok(GatePass {
            // @cpt-end:cpt-cf-oagw-algo-data-plane-pipeline-execution:ph-1:inst-inject
            effective,
            registry,
            request_id,
            auth_injections,
            response_headers,
            pinned_ip,
        })
    }
}

/// Whether a route `http_match` rule applies to `(method, path)`.
///
/// A rule matches when its optional method equals the request method and its
/// `path_pattern` matches the request path: literal segments must match
/// exactly (case-insensitively) and `{...}` placeholders match any single
/// segment (mirroring the route-pattern grammar).
#[must_use]
fn http_match_applies(rule: &RouteHttpMatch, method: &str, path: &str) -> bool {
    if let Some(m) = &rule.method
        && !m.eq_ignore_ascii_case(method)
    {
        return false;
    }
    let pattern: Vec<&str> = rule
        .path_pattern
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if pattern.is_empty() {
        return segments.is_empty();
    }
    if pattern.len() != segments.len() {
        return false;
    }
    pattern
        .iter()
        .zip(segments.iter())
        .all(|(p, s)| (p.starts_with('{') && p.ends_with('}')) || p.eq_ignore_ascii_case(s))
}

/// Maps a domain error to a gateway problem response.
fn err_from_domain(status: u16, e: DomainError, _fallback_title: &str) -> ProblemResponse {
    // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-classify
    let (error_type, title, detail) = match &e {
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-gateway-source
        DomainError::AuthFailed(d) => (error_types::AUTH_FAILED, "Unauthorized", d.clone()),
        // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-gateway-source
        DomainError::CredentialError(d) => (error_types::AUTH_FAILED, "Unauthorized", d.clone()),
        DomainError::GuardRejected(d) => (error_types::VALIDATION, "Bad Request", d.clone()),
        DomainError::SsrfBlocked(d) => (error_types::VALIDATION, "SSRF blocked", d.clone()),
        DomainError::RateLimited => (error_types::RATE_LIMIT, "Too many requests", e.to_string()),
        DomainError::PepDenied(d) => (error_types::AUTH_FAILED, "Forbidden", d.clone()),
        DomainError::Validation { detail } => {
            (error_types::VALIDATION, "Bad Request", detail.clone())
        }
        _ => {
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal
            // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal-source
            (error_types::INTERNAL, "Internal", e.to_string())
            // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal
            // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal-source
        }
    };
    // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-gateway-map
    let mut resp = ProblemResponse::new(status.max(400), error_type, title, detail);
    // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-gateway-map
    // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-gateway-source-header
    resp.error_source = "gateway";
    // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-gateway-source-header
    // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-classify
    resp
}

/// Maps a PEP enforcer error to a gateway problem response.
#[must_use]
pub fn pep_error(e: authz_resolver_sdk::pep::EnforcerError) -> ProblemResponse {
    // @cpt-begin:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep-deny-return
    match e {
        // @cpt-end:cpt-cf-oagw-flow-data-plane-proxy-request:ph-1:inst-pep-deny-return
        authz_resolver_sdk::pep::EnforcerError::Denied { deny_reason } => {
            let detail = deny_reason
                .as_ref()
                .and_then(|r| r.details.clone())
                .unwrap_or_else(|| "authorization denied".to_owned());
            let mut resp = ProblemResponse::new(403, error_types::AUTH_FAILED, "Forbidden", detail);
            resp.error_source = "gateway";
            resp
        }
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal
        // @cpt-begin:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal-source
        other => {
            // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal
            // @cpt-end:cpt-cf-oagw-algo-data-plane-error-mapping:ph-1:inst-internal-source
            let mut resp = ProblemResponse::new(
                500,
                error_types::INTERNAL,
                "Internal",
                format!("PEP evaluation failed: {other}"),
            );
            resp.error_source = "gateway";
            resp
        }
    }
}

/// Picks a free ephemeral TCP port on loopback.
#[must_use]
pub fn pick_loopback_port() -> u16 {
    TcpListener::bind(SocketAddr::from((IpAddr::from([127, 0, 0, 1]), 0)))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// Shutdown watch wiring [`ProxyBridgeHandle::stop`] into pingora's
/// `Server::run`: it resolves when the bridge's watch channel is flipped
/// (or its sender is dropped), signalling a fast shutdown so the server,
/// its runtimes and its listener all terminate.
struct BridgeShutdownWatch(tokio::sync::Mutex<tokio::sync::watch::Receiver<bool>>);

#[async_trait]
impl ShutdownSignalWatch for BridgeShutdownWatch {
    async fn recv(&self) -> ShutdownSignal {
        // Resolves once `stop()` replaces the value or the handle (and thus
        // the sender) is dropped — either way the server should terminate.
        let mut rx = self.0.lock().await;
        let _ = rx.changed().await;
        ShutdownSignal::FastShutdown
    }
}

/// Handle to the background pingora proxy bridge.
pub struct ProxyBridgeHandle {
    /// The internal listener port the axum relay forwards to.
    pub port: u16,
    /// The per-bridge relay secret the surface must stamp on every internal
    /// request (verified by the pingora gate; loopback port spoofing is
    /// therefore impossible without it).
    pub relay_secret: String,
    /// Dedicated thread running the pingora server.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Flip on [`Self::stop`] to terminate the pingora server.
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
}

/// Spawns the pingora `Server` hosting [`DataPlaneService`] on an ephemeral
/// loopback port.
///
/// The server runs its own runtimes on a dedicated thread (pingora fans out
/// its own worker runtimes); the axum relay talks to it over plain TCP. A
/// fresh relay secret is minted per spawn and installed on the service, and
/// the returned handle drives a real shutdown through pingora's signal watch
/// (no ghost listener after [`ProxyBridgeHandle::stop`]).
#[must_use]
pub fn spawn_proxy_bridge(service: DataPlaneService) -> Option<ProxyBridgeHandle> {
    let port = pick_loopback_port();
    let addr = format!("127.0.0.1:{port}");
    // Random per-bridge bearer credential (uuid v4); the axum surface stamps
    // it on every internal request and the gate verifies it before trusting
    // any identity header.
    let relay_secret = uuid::Uuid::new_v4().to_string();
    let service = service.with_relay_secret(relay_secret.clone());

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(true);
    let thread = std::thread::Builder::new()
        .name("oagw-pingora".to_owned())
        .spawn(move || {
            // Server::run creates and drives its own worker runtimes
            // internally; no tokio handle needs to be imported here. Unlike
            // run_forever (which calls process::exit(0)), `run` returns once
            // the shutdown watch fires, so stop() can join the thread.
            let mut server = match Server::new(None) {
                Ok(s) => s,
                Err(e) => {
                    error!("oagw: pingora server init failed: {e}");
                    return;
                }
            };
            server.bootstrap();
            let conf = server.configuration.clone();
            let mut proxy_service = http_proxy_service(&conf, service);
            proxy_service.add_tcp(&addr);
            server.add_service(proxy_service);
            server.run(RunArgs {
                shutdown_signal: Box::new(BridgeShutdownWatch(tokio::sync::Mutex::new(
                    shutdown_rx,
                ))),
            });
            info!(port, "oagw data-plane proxy bridge stopped");
        })
        .map_err(|e| error!("oagw: failed to spawn pingora thread: {e}"))
        .ok()?;

    info!(
        port,
        "oagw data-plane proxy bridge bound on 127.0.0.1:{port}"
    );
    Some(ProxyBridgeHandle {
        port,
        relay_secret,
        thread: Some(thread),
        shutdown_tx: Some(shutdown_tx),
    })
}

impl ProxyBridgeHandle {
    /// Stops the bridge: signals the pingora server to shut down (releasing
    /// the listener and worker runtimes) and joins its thread. Idempotent.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(false);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::http_match_applies;
    use crate::domain::models::RouteHttpMatch;

    fn rule(method: Option<&str>, pattern: &str) -> RouteHttpMatch {
        RouteHttpMatch {
            method: method.map(str::to_owned),
            path_pattern: pattern.to_owned(),
        }
    }

    #[test]
    fn http_match_placeholders_and_methods() {
        let r = rule(None, "/orders/{id}");
        assert!(http_match_applies(&r, "GET", "/orders/123"));
        assert!(
            !http_match_applies(&r, "GET", "/orders/123/items"),
            "extra segment"
        );
        assert!(
            !http_match_applies(&r, "GET", "/products/123"),
            "literal mismatch"
        );

        let m = rule(Some("POST"), "/orders/{id}");
        assert!(http_match_applies(&m, "POST", "/orders/9"));
        assert!(
            !http_match_applies(&m, "GET", "/orders/9"),
            "method mismatch"
        );
        assert!(
            http_match_applies(&m, "post", "/orders/9"),
            "case-insensitive method"
        );
    }

    #[test]
    fn http_match_exact_path_rules() {
        let r = rule(None, "/healthz");
        assert!(http_match_applies(&r, "GET", "/healthz"));
        assert!(!http_match_applies(&r, "GET", "/healthz/extra"));
    }
}
