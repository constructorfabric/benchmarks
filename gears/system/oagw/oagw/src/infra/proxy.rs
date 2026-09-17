//! Outbound proxy pipeline — the OAGW data plane (DOCS §3.3).
//!
//! [`ProxyService::handle`] is transport-agnostic: it consumes a
//! [`ProxyRequest`] and produces a [`ProxyResponse`], so the REST layer
//! adapts axum to it and unit tests drive it directly.
//!
//! Pipeline (per request):
//!
//! 1. CORS preflight short-circuit — permissive 204 with no upstream
//!    resolution and no tenant context (ADR 0004).
//! 2. [`DataPlaneService::resolve`] — alias → shadowing upstream → pool
//!    member → route → merged rate limit / CORS / headers / auth / plugins.
//! 3. CORS enforcement on actual cross-origin requests (403).
//! 4. Request rate limiting.
//! 5. Header assembly: hop-by-hop strip, passthrough policy (`all` /
//!    `allowlist` / `none`), remove/set/add transforms, `Host` replaced
//!    with the upstream authority.
//! 6. Auth block, then the plugin chain (request phase). Guards validate
//!    the client's inbound headers (ADR 0009) and their rejections
//!    short-circuit with 400 / 502; auth/transform mutate the outbound set.
//! 7. Forward via the shared toolkit-http client (retries disabled,
//!    response body capped by `max_response_body_bytes`).
//! 8. Response phase: hop-by-hop strip, transforms, plugin chain, CORS
//!    response headers.
//! 9. `X-OAGW-Error-Source: upstream` on every passthrough response;
//!    OAGW-generated problem responses carry `gateway` instead (ADR 0007 —
//!    set by the REST error mapping).
//!
//! SSRF host screening (config `ssrf_policy`) applies to IP-literal
//! endpoint hosts only; hostname resolution is deliberately not performed
//! on the hot path (DESIGN §Security Considerations).

use std::sync::Arc;

use bytes::Bytes;
use http::header::{
    HeaderMap, HeaderName, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN,
    TRANSFER_ENCODING,
};
use http::{Method, StatusCode};
use toolkit_http::{HttpClient, HttpError};
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::error::{DataPlaneError, ErrorExtensions};
use crate::domain::models::{Endpoint, EndpointScheme, PassthroughMode, RateLimitScope};
use crate::domain::plugin::{GuardDecision, PluginContext};
use crate::domain::ratelimit::RateLimitDecision;
use crate::domain::services::data_plane::{DataPlaneService, Resolution};
use crate::infra::cors;
use crate::infra::plugins::{PluginRegistry, ResolvedBuiltin, ResolvedPlugin};
use crate::infra::ratelimit::RateLimiter;

/// `X-OAGW-Error-Source` header — present on every gateway response.
pub const X_OAGW_ERROR_SOURCE: &str = "x-oagw-error-source";
/// `X-OAGW-Target-Host` routing header (DOCS §7.3).
pub const X_OAGW_TARGET_HOST: &str = "x-oagw-target-host";
/// Error source: OAGW generated the (problem+json) response.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Error source: the response is a pass-through from the upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Hard request-body cap (DOCS body validation: reject before buffering).
pub const MAX_REQUEST_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Hop-by-hop headers (RFC 9110 §7.6.1) plus the routing headers OAGW
/// consumes and never forwards, lowercased.
const STRIPPED_INBOUND: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    X_OAGW_TARGET_HOST,
];

/// Where a response originated: OAGW itself or the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    Gateway,
    Upstream,
}

impl ErrorSource {
    /// Wire value (`gateway` / `upstream`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => ERROR_SOURCE_GATEWAY,
            Self::Upstream => ERROR_SOURCE_UPSTREAM,
        }
    }
}

/// Transport-agnostic inbound proxy request.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// HTTP method to forward.
    pub method: Method,
    /// Absolute request path beginning with `/` (percent-encoded as
    /// received).
    pub path: String,
    /// Raw query string (without `?`) when present.
    pub query: Option<String>,
    /// Inbound headers (routing headers are stripped during assembly).
    pub headers: HeaderMap,
    /// Buffered request body (validated against the 100 MB cap).
    pub body: Bytes,
}

/// Transport-agnostic outbound proxy response.
#[derive(Debug, Clone)]
pub struct ProxyResponse {
    /// Status to return to the client.
    pub status: StatusCode,
    /// Headers to return (already includes `X-OAGW-Error-Source`).
    pub headers: HeaderMap,
    /// Body to return.
    pub body: Bytes,
    /// Whether this response originated at OAGW or upstream.
    pub error_source: ErrorSource,
}

impl ProxyResponse {
    /// A response generated locally by OAGW (preflight, etc.).
    #[must_use]
    pub fn gateway(status: StatusCode, headers: HeaderMap, body: Bytes) -> Self {
        let mut headers = headers;
        headers.insert(
            error_source_header(),
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        Self {
            status,
            headers,
            body,
            error_source: ErrorSource::Gateway,
        }
    }
}

/// The outbound proxy pipeline.
pub struct ProxyService {
    data_plane: Arc<DataPlaneService>,
    plugins: Arc<PluginRegistry>,
    rate_limiter: Arc<RateLimiter>,
    cred_store: Arc<dyn credstore_sdk::CredStoreClientV1>,
    http: HttpClient,
    config: Arc<OagwConfig>,
}

/// One lifecycle phase of a plugin run.
#[derive(Debug, Clone, Copy)]
enum PluginPhase {
    Request,
    Response,
}

/// A buffered upstream response, pre-header-processing.
struct Forwarded {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl ProxyService {
    /// Build the pipeline over the given resolution/plugin infrastructure.
    #[must_use]
    pub fn new(
        data_plane: Arc<DataPlaneService>,
        plugins: Arc<PluginRegistry>,
        rate_limiter: Arc<RateLimiter>,
        cred_store: Arc<dyn credstore_sdk::CredStoreClientV1>,
        http: HttpClient,
        config: Arc<OagwConfig>,
    ) -> Self {
        Self {
            data_plane,
            plugins,
            rate_limiter,
            cred_store,
            http,
            config,
        }
    }

    /// Execute the proxy pipeline for one request.
    ///
    /// # Errors
    /// `DataPlaneError` for every gateway-side failure (resolution, CORS,
    /// rate limit, plugin, transport); upstream HTTP responses pass back
    /// as successful values regardless of their status.
    pub async fn handle(
        &self,
        ctx: &SecurityContext,
        req: ProxyRequest,
        alias: &str,
    ) -> Result<ProxyResponse, DataPlaneError> {
        let ext = |inline: ErrorExtensions| {
            inline.with_alias(alias).with_path(req.path.as_str())
        };

        // 1. CORS preflight: no resolution, no tenant context (ADR 0004).
        if cors::is_preflight(&req.method, &req.headers) {
            let headers = cors::preflight_headers(&req.headers);
            return Ok(ProxyResponse::gateway(
                StatusCode::NO_CONTENT,
                headers,
                Bytes::new(),
            ));
        }

        // 2. Resolve the target; a real forward needs a matching route.
        let target_host = req
            .headers
            .get(target_host_header())
            .and_then(|v| v.to_str().ok());
        let resolution = self
            .data_plane
            .resolve(ctx, alias, &req.method, &req.path, target_host)
            .await?;
        if resolution.route.is_none() {
            return Err(DataPlaneError::RouteNotFound {
                detail: format!(
                    "no route matches {} {} on upstream '{alias}'",
                    req.method, req.path
                ),
                extensions: ext(ErrorExtensions::default()),
            });
        }

        // 3. CORS enforcement on actual cross-origin requests.
        let origin = req.headers.get(ORIGIN).and_then(|v| v.to_str().ok());
        let cors_headers = match &resolution.cors {
            Some(config) => cors::enforce_actual(origin, req.method.as_str(), config)?,
            None => None,
        };

        // 4. Rate limiting (merged limits drop the scope, so enforcement
        //    keys per tenant + alias).
        if let Some(limit) = &resolution.rate_limit {
            let decision = self.rate_limiter.check(
                RateLimiter::key(
                    &resolution.upstream.alias,
                    ctx.subject_tenant_id(),
                    RateLimitScope::Tenant,
                ),
                limit,
            );
            if let RateLimitDecision::Deny { retry_after_seconds } = decision {
                return Err(DataPlaneError::RateLimitExceeded {
                    retry_after_seconds,
                    extensions: ext(ErrorExtensions::default().with_retry_after(retry_after_seconds)),
                });
            }
        }

        // 5. Build outbound headers (passthrough policy + transforms).
        let mut outbound = assemble_request_headers(&req, &resolution);

        // 6. Auth block, then the plugin chain (request phase).
        if let Some(auth) = &resolution.auth {
            let resolved = self
                .plugins
                .resolve_auth(&auth.plugin_type, auth.config.clone())?;
            self.run_plugin(PluginPhase::Request, &resolved, ctx, &mut outbound, &ext)
                .await?;
        }
        for binding in &resolution.plugins {
            let resolved = self.plugins.resolve(binding).await?;
            // Guards validate the client's inbound request (ADR 0009:
            // validation is independent of passthrough config), while auth
            // and transform plugins mutate the outbound set — so guards get
            // the pre-assembly headers even when forwarding drops them.
            let decision = if matches!(resolved.plugin, ResolvedPlugin::Guard(_)) {
                let mut inbound = req.headers.clone();
                self.run_plugin(
                    PluginPhase::Request,
                    &resolved,
                    ctx,
                    &mut inbound,
                    &ext,
                )
                .await?
            } else {
                self.run_plugin(PluginPhase::Request, &resolved, ctx, &mut outbound, &ext)
                    .await?
            };
            reject(decision, &ext)?;
        }

        // 7. Forward to the upstream.
        let forward = self.forward(&req, &resolution, outbound).await?;

        // 8. Response phase: strip, transform, plugin chain, CORS.
        let mut response_headers = forward.headers;
        strip_response_headers(&mut response_headers);
        if let Some(headers) = &resolution.headers {
            apply_header_rules(&mut response_headers, &headers.response);
        }
        for binding in &resolution.plugins {
            let resolved = self.plugins.resolve(binding).await?;
            let decision = self
                .run_plugin(
                    PluginPhase::Response,
                    &resolved,
                    ctx,
                    &mut response_headers,
                    &ext,
                )
                .await?;
            reject(decision, &ext)?;
        }
        if let Some(cors_headers) = &cors_headers {
            cors::apply_response_headers(&mut response_headers, cors_headers);
        }
        response_headers.insert(
            error_source_header(),
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );

        Ok(ProxyResponse {
            status: forward.status,
            headers: response_headers,
            body: forward.body,
            error_source: ErrorSource::Upstream,
        })
    }

    /// Run one plugin phase hook with a context bound to this request.
    ///
    /// # Errors
    /// `DataPlaneError` when the hook fails.
    async fn run_plugin(
        &self,
        phase: PluginPhase,
        resolved: &ResolvedBuiltin,
        ctx: &SecurityContext,
        headers: &mut HeaderMap,
        ext: &(dyn Fn(ErrorExtensions) -> ErrorExtensions + Sync),
    ) -> Result<Option<GuardDecision>, DataPlaneError> {
        let pctx = PluginContext {
            security_context: ctx.clone(),
            cred_store: self.cred_store.clone(),
            http: self.http.clone(),
            config: resolved.config.clone(),
        };
        let guard = match phase {
            PluginPhase::Request => resolved.plugin.run_request(&pctx, headers).await,
            PluginPhase::Response => resolved.plugin.run_response(&pctx, headers).await,
        };
        guard.map_err(|e| e.into_data_plane(ext(ErrorExtensions::default())))
    }

    /// Build and send the outbound request, buffering the response.
    ///
    /// # Errors
    /// `DataPlaneError` on transport/build failures (upstream HTTP status
    /// codes pass through as values).
    async fn forward(
        &self,
        req: &ProxyRequest,
        resolution: &Resolution,
        headers: HeaderMap,
    ) -> Result<Forwarded, DataPlaneError> {
        let url = self.build_url(resolution, &req.path, req.query.as_deref())?;
        let builder = match req.method {
            Method::GET => self.http.get(&url),
            Method::POST => self.http.post(&url),
            Method::PUT => self.http.put(&url),
            Method::PATCH => self.http.patch(&url),
            Method::DELETE => self.http.delete(&url),
            Method::HEAD => self.http.head(&url),
            Method::OPTIONS => self.http.options(&url),
            ref other => {
                return Err(DataPlaneError::Validation {
                    detail: format!("HTTP method '{other}' is not supported by the proxy"),
                    extensions: ext_for(&resolution.upstream.alias, &req.path),
                });
            }
        };
        let resp = builder
            .headers(header_pairs(&headers))
            .body_bytes(req.body.clone())
            .send()
            .await
            .map_err(|e| map_forward_error(e, resolution, &req.path))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp
            .bytes()
            .await
            .map_err(|e| map_forward_error(e, resolution, &req.path))?;
        Ok(Forwarded { status, headers, body })
    }

    /// Build the outbound URL for a resolution.
    #[allow(clippy::result_large_err)] // rich RFC 9457 error carrier by design
    fn build_url(
        &self,
        resolution: &Resolution,
        path: &str,
        query: Option<&str>,
    ) -> Result<String, DataPlaneError> {
        let endpoint = &resolution.endpoint;
        let scheme = match endpoint.scheme {
            EndpointScheme::Https => "https",
            EndpointScheme::Http => {
                if !self.config.allow_http_upstream {
                    return Err(DataPlaneError::Validation {
                        detail: "http upstream endpoints are disabled by configuration".to_owned(),
                        extensions: ext_for(&resolution.upstream.alias, path),
                    });
                }
                "http"
            }
            EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
                let name = match endpoint.scheme {
                    EndpointScheme::Wss => "wss",
                    EndpointScheme::Wt => "wt",
                    EndpointScheme::Grpc => "grpc",
                    _ => unreachable!(),
                };
                return Err(DataPlaneError::Validation {
                    detail: format!(
                        "endpoint scheme '{name}' is not supported by the HTTP proxy"
                    ),
                    extensions: ext_for(&resolution.upstream.alias, path),
                });
            }
        };
        if !self.ssrf_allows(endpoint) {
            return Err(DataPlaneError::Validation {
                detail: format!("ssrf policy blocks endpoint host '{}'", endpoint.host),
                extensions: ext_for(&resolution.upstream.alias, path),
            });
        }
        let authority = upstream_authority(endpoint);
        let query = query.map(|q| format!("?{q}")).unwrap_or_default();
        Ok(format!("{scheme}://{authority}{path}{query}"))
    }

    /// Whether the endpoint host passes the SSRF policy. Only IP-literal
    /// hosts are screened (design decision: no DNS on the hot path).
    fn ssrf_allows(&self, endpoint: &Endpoint) -> bool {
        if !self.config.ssrf_policy.enabled {
            return true;
        }
        if self
            .config
            .ssrf_policy
            .allowlist
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&endpoint.host))
        {
            return true;
        }
        let Ok(ip) = endpoint.host.parse::<std::net::IpAddr>() else {
            return true; // hostname — not screened on the hot path
        };
        match ip {
            std::net::IpAddr::V4(v4) => {
                !(v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified())
            }
            std::net::IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified()),
        }
    }
}

// ---------------------------------------------------------------------------
// Header assembly
// ---------------------------------------------------------------------------

/// Assemble the outbound request headers from the inbound request and the
/// upstream's header rules.
fn assemble_request_headers(req: &ProxyRequest, resolution: &Resolution) -> HeaderMap {
    let passthrough = resolution
        .headers
        .as_ref()
        .map_or(PassthroughMode::None, |h| h.request.passthrough);
    let allowlist: Vec<&str> = resolution
        .headers
        .as_ref()
        .map(|h| {
            h.request
                .passthrough_allowlist
                .iter()
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();

    let mut out = HeaderMap::new();
    for (name, value) in &req.headers {
        let name_lc = name.as_str();
        if STRIPPED_INBOUND.contains(&name_lc) {
            continue;
        }
        let keep = match passthrough {
            PassthroughMode::All => true,
            PassthroughMode::None => name_lc == CONTENT_TYPE.as_str(),
            PassthroughMode::Allowlist => {
                name_lc == CONTENT_TYPE.as_str()
                    || allowlist.iter().any(|a| a.eq_ignore_ascii_case(name_lc))
            }
        };
        if keep {
            out.append(name.clone(), value.clone());
        }
    }
    if let Some(headers) = &resolution.headers {
        apply_header_rules(&mut out, &headers.request);
    }
    if let Ok(host) = HeaderValue::from_str(&upstream_authority(&resolution.endpoint)) {
        out.insert(HOST, host);
    }
    out
}

/// Apply `remove` / `set` / `add` rules to a header map.
fn apply_header_rules<R: HeaderRules>(headers: &mut HeaderMap, rules: &R) {
    for name in rules.remove_names() {
        if let Ok(n) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&n);
        }
    }
    for (name, value) in rules.set_pairs() {
        insert_pair(headers, name, value);
    }
    for (name, value) in rules.add_pairs() {
        append_pair(headers, name, value);
    }
}

/// Uniform view over the request/response header rule structs.
trait HeaderRules {
    fn remove_names(&self) -> &[String];
    fn set_pairs(&self) -> &std::collections::BTreeMap<String, String>;
    fn add_pairs(&self) -> &std::collections::BTreeMap<String, String>;
}

impl HeaderRules for crate::domain::models::RequestHeaderRules {
    fn remove_names(&self) -> &[String] {
        &self.remove
    }
    fn set_pairs(&self) -> &std::collections::BTreeMap<String, String> {
        &self.set
    }
    fn add_pairs(&self) -> &std::collections::BTreeMap<String, String> {
        &self.add
    }
}

impl HeaderRules for crate::domain::models::ResponseHeaderRules {
    fn remove_names(&self) -> &[String] {
        &self.remove
    }
    fn set_pairs(&self) -> &std::collections::BTreeMap<String, String> {
        &self.set
    }
    fn add_pairs(&self) -> &std::collections::BTreeMap<String, String> {
        &self.add
    }
}

/// Insert (replace) a header pair; invalid names/values are dropped.
fn insert_pair(headers: &mut HeaderMap, name: &str, value: &str) {
    let Ok(n) = HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    let Ok(v) = HeaderValue::from_str(value) else {
        return;
    };
    headers.insert(n, v);
}

/// Append a header pair; invalid names/values are dropped.
fn append_pair(headers: &mut HeaderMap, name: &str, value: &str) {
    let Ok(n) = HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    let Ok(v) = HeaderValue::from_str(value) else {
        return;
    };
    headers.append(n, v);
}

/// Strip hop-by-hop + framing headers from an upstream response.
fn strip_response_headers(headers: &mut HeaderMap) {
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        CONTENT_LENGTH.as_str(),
        // The client decompressed the body; drop the encoding label so
        // plain bytes are not served with a `gzip` header.
        "content-encoding",
    ] {
        if let Ok(n) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(n);
        }
    }
}

/// Convert a `HeaderMap` into the toolkit-http header pair list.
fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// `host` or `host:port` for a non-default endpoint port.
fn upstream_authority(endpoint: &Endpoint) -> String {
    let port = endpoint.effective_port();
    if port == endpoint.scheme.default_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{port}", endpoint.host)
    }
}

fn error_source_header() -> HeaderName {
    HeaderName::from_static(X_OAGW_ERROR_SOURCE)
}

fn target_host_header() -> HeaderName {
    HeaderName::from_static(X_OAGW_TARGET_HOST)
}

// ---------------------------------------------------------------------------
// Inbound / error mapping
// ---------------------------------------------------------------------------

/// Validate framing headers against the buffered body (DOCS body
/// validation): `Transfer-Encoding` must be `chunked` (the only supported
/// encoding) and `Content-Length`, when present, must match the buffered
/// size.
///
/// # Errors
/// `Validation` (400) on a non-integer or mismatched `Content-Length` or
/// an unsupported `Transfer-Encoding`.
#[allow(clippy::result_large_err)] // rich RFC 9457 error carrier by design
pub fn validate_inbound(headers: &HeaderMap, body_len: usize) -> Result<(), DataPlaneError> {
    if let Some(te) = headers.get(TRANSFER_ENCODING) {
        let raw = te.to_str().map_err(|_| DataPlaneError::Validation {
            detail: "invalid Transfer-Encoding header".to_owned(),
            extensions: ErrorExtensions::default(),
        })?;
        let chunked = raw
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case("chunked"));
        if !chunked {
            return Err(DataPlaneError::Validation {
                detail: "unsupported Transfer-Encoding (only chunked is supported)".to_owned(),
                extensions: ErrorExtensions::default(),
            });
        }
    }
    if let Some(cl) = headers.get(CONTENT_LENGTH) {
        let raw = cl.to_str().map_err(|_| DataPlaneError::Validation {
            detail: "invalid Content-Length header".to_owned(),
            extensions: ErrorExtensions::default(),
        })?;
        let parsed = raw.trim().parse::<usize>().map_err(|_| {
            DataPlaneError::Validation {
                detail: "Content-Length must be a valid integer".to_owned(),
                extensions: ErrorExtensions::default(),
            }
        })?;
        if parsed != body_len {
            return Err(DataPlaneError::Validation {
                detail: format!(
                    "Content-Length {parsed} does not match actual body size {body_len}"
                ),
                extensions: ErrorExtensions::default(),
            });
        }
    }
    Ok(())
}

/// Short-circuit on a guard decision (request 400 / response 502).
#[allow(clippy::result_large_err)] // rich RFC 9457 error carrier by design
fn reject(
    decision: Option<GuardDecision>,
    ext: &dyn Fn(ErrorExtensions) -> ErrorExtensions,
) -> Result<(), DataPlaneError> {
    let Some(GuardDecision::Reject { status, detail }) = decision else {
        return Ok(());
    };
    if status == 400 {
        return Err(DataPlaneError::Validation {
            detail,
            extensions: ext(ErrorExtensions::default()),
        });
    }
    Err(DataPlaneError::Protocol {
        detail,
        extensions: ext(ErrorExtensions::default()),
    })
}

/// Map a toolkit-http failure onto the DOCS §8 error catalogue.
fn map_forward_error(err: HttpError, resolution: &Resolution, path: &str) -> DataPlaneError {
    match err {
        HttpError::Timeout(d) | HttpError::DeadlineExceeded(d) => {
            DataPlaneError::RequestTimeout {
                detail: format!("upstream request did not complete within {d:?}"),
                extensions: ext_for(&resolution.upstream.alias, path),
            }
        }
        HttpError::BodyTooLarge { limit, actual } => DataPlaneError::PayloadTooLarge {
            detail: format!("upstream response body {actual} bytes exceeds the {limit} byte limit"),
            extensions: ext_for(&resolution.upstream.alias, path),
        },
        HttpError::Transport(_) | HttpError::Tls(_) => DataPlaneError::LinkUnavailable {
            detail: format!("upstream link unavailable for '{}'", resolution.upstream.alias),
            retry_after: None,
            extensions: ext_for(&resolution.upstream.alias, path),
        },
        HttpError::InvalidUri { reason, .. } | HttpError::InvalidScheme { reason, .. } => {
            DataPlaneError::Validation {
                detail: format!("invalid upstream URL: {reason}"),
                extensions: ext_for(&resolution.upstream.alias, path),
            }
        }
        HttpError::Overloaded | HttpError::ServiceClosed => DataPlaneError::Internal {
            detail: format!("outbound client unavailable: {err}"),
            extensions: ext_for(&resolution.upstream.alias, path),
        },
        other => DataPlaneError::Internal {
            detail: format!("upstream request failed: {other}"),
            extensions: ext_for(&resolution.upstream.alias, path),
        },
    }
}

fn ext_for(alias: &str, path: &str) -> ErrorExtensions {
    ErrorExtensions::default().with_alias(alias).with_path(path)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::time::Duration;

    use uuid::Uuid;

    use crate::config::SsrfPolicy;
    use crate::domain::models::{
        AuthConfig, CorsConfig, CorsMethod, Endpoint, HttpMatch, PathSuffixMode, RateLimitConfig,
        RateLimitWindow, Route, RouteMatch, ServerConfig, SharingMode, Upstream, UpstreamProtocol,
    };
    use crate::infra::storage::MemoryStore;
    use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
    use authz_resolver_sdk::models::{
        Capability, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
    };
    use authz_resolver_sdk::pep::PolicyEnforcer;
    use authz_resolver_sdk::AuthZResolverClient;
    use tenant_resolver_sdk::models::{GetAncestorsResponse, TenantInfo, TenantStatus};
    use tenant_resolver_sdk::{
        GetAncestorsOptions, GetDescendantsOptions, GetTenantsOptions, IsAncestorOptions,
        TenantId, TenantResolverClient, TenantResolverError,
    };

    use crate::domain::repo::{RouteRepo, UpstreamRepo};
    use toolkit_http::{HttpClientBuilder, HttpClientConfig};
    use toolkit_security::pep_properties;
    use toolkit_security::SecurityContext;

    use crate::gts_helpers;

    const TENANT: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
    const SUBJECT: Uuid = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
    const UPSTREAM_ID: Uuid = Uuid::from_u128(0xaaaa_aaaa_aaaa_aaaa_aaaa_aaaa_aaaa_0001);

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(SUBJECT)
            .subject_tenant_id(TENANT)
            .build()
            .unwrap()
    }

    fn http_client(timeout_secs: u64) -> HttpClient {
        HttpClientBuilder::with_config(HttpClientConfig::proxy())
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .unwrap()
    }

    /// PEP mock mirroring static-authz: grants for any non-nil tenant.
    struct AllowTenant;
    #[async_trait::async_trait]
    impl AuthZResolverClient for AllowTenant {
        async fn evaluate(
            &self,
            req: EvaluationRequest,
        ) -> Result<EvaluationResponse, authz_resolver_sdk::AuthZResolverError> {
            let tid = req
                .subject
                .properties
                .get("tenant_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            let Some(tid) = tid else {
                return Ok(EvaluationResponse {
                    decision: false,
                    context: EvaluationResponseContext::default(),
                });
            };
            if tid == Uuid::default() {
                return Ok(EvaluationResponse {
                    decision: false,
                    context: EvaluationResponseContext::default(),
                });
            }
            Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints: vec![Constraint {
                        predicates: vec![Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            [tid],
                        ))],
                    }],
                    ..Default::default()
                },
            })
        }
    }

    /// Tenant resolver that reports no ancestors (single-tenant chain).
    struct NoAncestors;
    #[async_trait::async_trait]
    impl TenantResolverClient for NoAncestors {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            unimplemented!()
        }
        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            unimplemented!()
        }
        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            unimplemented!()
        }
        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            Ok(GetAncestorsResponse {
                tenant: tenant_resolver_sdk::models::TenantRef {
                    id: TenantId(TENANT),
                    status: TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                ancestors: Vec::new(),
            })
        }
        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<tenant_resolver_sdk::models::GetDescendantsResponse, TenantResolverError> {
            unimplemented!()
        }
        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor_id: TenantId,
            _descendant_id: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            unimplemented!()
        }
    }

    fn config() -> Arc<OagwConfig> {
        Arc::new(OagwConfig {
            proxy_timeout_secs: 30,
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicy {
                enabled: false,
                allowlist: Vec::new(),
            },
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10,
            max_response_body_bytes: 1024 * 1024,
        })
    }

    /// Shared store + mock server; `proxy()` rebuilds a pipeline over the
    /// current store contents so tests may mutate resources first.
    struct Harness {
        store: Arc<MemoryStore>,
        server: httpmock::MockServer,
        creds: Arc<dyn credstore_sdk::CredStoreClientV1>,
    }

    impl Harness {
        fn upstream(&self) -> Upstream {
            let base = self.server.base_url(); // http://127.0.0.1:<port>
            let (host, port) = base
                .trim_start_matches("http://")
                .rsplit_once(':')
                .unwrap();
            Upstream {
                id: UPSTREAM_ID,
                tenant_id: TENANT,
                enabled: true,
                alias: "vendor".to_owned(),
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme: EndpointScheme::Http,
                        host: host.to_owned(),
                        port: port.parse().ok(),
                    }],
                },
                protocol: UpstreamProtocol::Http,
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
            }
        }

        fn route() -> Route {
            Route {
                id: Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_0001),
                tenant_id: TENANT,
                tags: Vec::new(),
                upstream_id: UPSTREAM_ID,
                match_: RouteMatch {
                    http: Some(HttpMatch {
                        methods: vec![
                            crate::domain::models::HttpMethod::Post,
                            crate::domain::models::HttpMethod::Get,
                        ],
                        path: "/v1".to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                plugins: None,
                rate_limit: None,
                cors: None,
            }
        }

        async fn new() -> Self {
            let server = httpmock::MockServer::start();
            let store = Arc::new(MemoryStore::new());
            let h = Self {
                store,
                server,
                creds: Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
            };
            h.seed(h.upstream(), Self::route()).await;
            h
        }

        async fn seed(&self, upstream: Upstream, route: Route) {
            self.store.clone().upstreams().insert(upstream).await.unwrap();
            self.store.clone().routes().insert(route).await.unwrap();
        }

        /// Rebuild a pipeline over the store's current contents.
        fn proxy(&self) -> ProxyService {
            self.proxy_with_timeout(5)
        }

        /// Like [`Harness::proxy`] but with a custom outbound timeout.
        fn proxy_with_timeout(&self, timeout_secs: u64) -> ProxyService {
            let data_plane = Arc::new(DataPlaneService::new(
                Arc::new(self.store.clone().upstreams()),
                Arc::new(self.store.clone().routes()),
                Arc::new(
                    PolicyEnforcer::new(Arc::new(AllowTenant))
                        .with_capabilities(vec![Capability::TenantHierarchy]),
                ),
                Arc::new(NoAncestors),
            ));
            let plugins = Arc::new(PluginRegistry::with_builtins(
                Duration::from_mins(5),
                10,
                Arc::new(self.store.clone().plugins()),
            ));
            ProxyService::new(
                data_plane,
                plugins,
                Arc::new(RateLimiter::with_capacity(100)),
                self.creds.clone(),
                http_client(timeout_secs),
                config(),
            )
        }

        fn request(method: Method, path: &str) -> ProxyRequest {
            ProxyRequest {
                method,
                path: path.to_owned(),
                query: None,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            }
        }
    }

    #[tokio::test]
    async fn forwards_request_end_to_end_with_upstream_source() {
        let h = Harness::new().await;
        let mock = h.server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/chat");
            then.status(200).body("{\"ok\":true}");
        });
        let mut req = Harness::request(Method::POST, "/v1/chat");
        req.body = Bytes::from_static(b"{\"hi\":1}");
        req.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let resp = h.proxy().handle(&ctx(), req, "vendor").await.unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.body, Bytes::from_static(b"{\"ok\":true}"));
        assert_eq!(resp.error_source, ErrorSource::Upstream);
        assert_eq!(
            resp.headers
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some(ERROR_SOURCE_UPSTREAM)
        );
        mock.assert();
    }

    #[tokio::test]
    async fn forwards_query_string() {
        let h = Harness::new().await;
        let mock = h.server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/search")
                .query_param("q", "cats");
            then.status(200).body("");
        });
        let mut req = Harness::request(Method::GET, "/v1/search");
        req.query = Some("q=cats".to_owned());
        h.proxy().handle(&ctx(), req, "vendor").await.unwrap();
        mock.assert();
    }

    #[tokio::test]
    async fn headers_passthrough_rules_and_routing_strip() {
        let h = Harness::new().await;
        let mock = h.server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1")
                .header_exists("x-fwd") // allowlisted: forwarded
                .header_exists("x-set") // injected by the set rule
                .header_missing("x-api-key") // not allowlisted: dropped
                .header_missing("x-drop") // removed by the remove rule
                .header_missing("x-oagw-target-host"); // routing header never forwards
            then.status(200).body("");
        });
        let mut upstream = h.upstream();
        upstream.headers = Some(crate::domain::models::HeadersConfig {
            request: crate::domain::models::RequestHeaderRules {
                set: std::collections::BTreeMap::from([("x-set".to_owned(), "1".to_owned())]),
                add: std::collections::BTreeMap::new(),
                remove: vec!["x-drop".to_owned()],
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["x-fwd".to_owned()],
            },
            response: crate::domain::models::ResponseHeaderRules::default(),
        });
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();

        let mut req = Harness::request(Method::GET, "/v1");
        req.headers
            .insert("x-fwd", HeaderValue::from_static("kept"));
        req.headers
            .insert("x-drop", HeaderValue::from_static("dropped"));
        req.headers
            .insert("x-api-key", HeaderValue::from_static("secret"));
        // A matching target-host so resolution passes; it must still not
        // be forwarded upstream.
        let ep = &h.upstream().server.endpoints[0];
        let target = format!("{}:{}", ep.host, ep.port.unwrap());
        req.headers.insert(
            X_OAGW_TARGET_HOST,
            HeaderValue::from_str(&target).expect("valid target host"),
        );
        h.proxy().handle(&ctx(), req, "vendor").await.unwrap();
        mock.assert();
    }

    #[tokio::test]
    async fn preflight_is_permissive_204_without_resolution() {
        let h = Harness::new().await;
        let mut req = Harness::request(Method::OPTIONS, "/ghost");
        req.headers
            .insert(ORIGIN, HeaderValue::from_static("https://app.example.com"));
        req.headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        let resp = h
            .proxy()
            .handle(&SecurityContext::anonymous(), req, "ghost")
            .await
            .unwrap();
        assert_eq!(resp.status, StatusCode::NO_CONTENT);
        assert_eq!(resp.error_source, ErrorSource::Gateway);
        assert_eq!(
            resp.headers
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            resp.headers
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );
    }

    #[tokio::test]
    async fn unknown_alias_is_route_not_found() {
        let h = Harness::new().await;
        let err = h
            .proxy()
            .handle(&ctx(), Harness::request(Method::GET, "/v1"), "nope")
            .await
            .unwrap_err();
        assert!(matches!(err, DataPlaneError::RouteNotFound { .. }));
    }

    #[tokio::test]
    async fn missing_route_match_is_404() {
        let h = Harness::new().await;
        // Route requires POST/GET on /v1: DELETE does not match.
        let err = h
            .proxy()
            .handle(&ctx(), Harness::request(Method::DELETE, "/v1"), "vendor")
            .await
            .unwrap_err();
        assert!(matches!(err, DataPlaneError::RouteNotFound { .. }));
    }

    #[tokio::test]
    async fn cors_actual_request_rejects_disallowed_origin_as_403() {
        let h = Harness::new().await;
        let mut upstream = h.upstream();
        upstream.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://good.example.com".to_owned()],
            allowed_methods: vec![CorsMethod::Post],
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();

        let mut req = Harness::request(Method::POST, "/v1/chat");
        req.headers
            .insert(ORIGIN, HeaderValue::from_static("https://evil.example.com"));
        let err = h
            .proxy()
            .handle(&ctx(), req, "vendor")
            .await
            .unwrap_err();
        assert!(matches!(err, DataPlaneError::CorsOriginNotAllowed { .. }));
    }

    #[tokio::test]
    async fn cors_allows_origin_and_adds_response_headers() {
        let h = Harness::new().await;
        let mock = h.server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1");
            then.status(200).body("");
        });
        let mut upstream = h.upstream();
        upstream.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://good.example.com".to_owned()],
            allowed_methods: vec![CorsMethod::Post],
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: true,
        });
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();

        let mut req = Harness::request(Method::POST, "/v1");
        req.headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://good.example.com"),
        );
        let resp = h.proxy().handle(&ctx(), req, "vendor").await.unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(
            resp.headers
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://good.example.com")
        );
        assert_eq!(
            resp.headers
                .get(http::header::ACCESS_CONTROL_EXPOSE_HEADERS)
                .and_then(|v| v.to_str().ok()),
            Some("X-Request-ID")
        );
        mock.assert();
    }

    #[tokio::test]
    async fn rate_limit_exceeded_returns_429_with_retry_after() {
        let h = Harness::new().await;
        let mut upstream = h.upstream();
        upstream.rate_limit = Some(RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: crate::domain::models::RateLimitAlgorithm::TokenBucket,
            sustained: crate::domain::models::SustainedRate {
                rate: 1,
                window: RateLimitWindow::Minute,
            },
            burst: Some(crate::domain::models::BurstConfig { capacity: 1 }),
            scope: crate::domain::models::RateLimitScope::Tenant,
            strategy: crate::domain::models::RateLimitStrategy::Reject,
            cost: 1,
        });
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();
        h.server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1");
            then.status(200).body("");
        });

        let proxy = h.proxy();
        // First request consumes the only token.
        proxy
            .handle(&ctx(), Harness::request(Method::GET, "/v1"), "vendor")
            .await
            .unwrap();
        let err = proxy
            .handle(&ctx(), Harness::request(Method::GET, "/v1"), "vendor")
            .await
            .unwrap_err();
        match err {
            DataPlaneError::RateLimitExceeded {
                retry_after_seconds, ..
            } => assert!(retry_after_seconds >= 1),
            other => panic!("expected rate limit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apikey_auth_injects_resolved_secret() {
        let h = Harness::new().await;
        let mut upstream = h.upstream();
        upstream.auth = Some(AuthConfig {
            plugin_type: gts_helpers::AUTH_PLUGIN_APIKEY.to_owned(),
            sharing: SharingMode::Private,
            config: serde_json::json!({
                "header": "x-api-key",
                "secret_ref": "openai-key",
            }),
        });
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();

        // Rebuild the harness with a cred store that knows the secret, then
        // register the mock (its lifetime borrows `h.server`).
        let h = Harness {
            creds: Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
                ("openai-key".to_owned(), "sk-secret".to_owned()),
            ])),
            ..h
        };
        let mock = h.server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat")
                .header("x-api-key", "sk-secret");
            then.status(200).body("");
        });
        let mut req = Harness::request(Method::POST, "/v1/chat");
        req.body = Bytes::from_static(b"{}");
        h.proxy().handle(&ctx(), req, "vendor").await.unwrap();
        mock.assert();
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_with_400() {
        let h = Harness::new().await;
        let mut upstream = h.upstream();
        upstream.plugins = Some(crate::domain::models::PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![crate::domain::models::PluginItem::Configured {
                plugin_ref: crate::domain::models::PluginRef::BuiltinId(
                    gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned(),
                ),
                config: serde_json::json!({ "required_request_headers": "x-correlation-id" }),
            }],
        });
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();

        let err = h
            .proxy()
            .handle(&ctx(), Harness::request(Method::GET, "/v1"), "vendor")
            .await
            .unwrap_err();
        let status = err.status();
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn required_headers_guard_accepts_client_header_without_passthrough_config() {
        // ADR 0009: guards validate the inbound request, independent of the
        // forwarding config — a client-supplied required header passes even
        // with the default `passthrough: none` (which would drop it from the
        // outbound set).
        let h = Harness::new().await;
        let mock = h.server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1");
            then.status(200).body("ok");
        });
        let mut upstream = h.upstream();
        upstream.plugins = Some(crate::domain::models::PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![crate::domain::models::PluginItem::Configured {
                plugin_ref: crate::domain::models::PluginRef::BuiltinId(
                    gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned(),
                ),
                config: serde_json::json!({ "required_request_headers": "x-correlation-id" }),
            }],
        });
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();

        let mut req = Harness::request(Method::GET, "/v1");
        req.headers.insert(
            "x-correlation-id",
            http::header::HeaderValue::from_static("abc"),
        );
        let resp = h.proxy().handle(&ctx(), req, "vendor").await.unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        mock.assert();
    }

    #[tokio::test]
    async fn upstream_5xx_passes_through_with_upstream_source() {
        let h = Harness::new().await;
        let mock = h.server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1");
            then.status(500).body("upstream boom");
        });
        let resp = h
            .proxy()
            .handle(&ctx(), Harness::request(Method::GET, "/v1"), "vendor")
            .await
            .unwrap();
        assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.body, Bytes::from_static(b"upstream boom"));
        assert_eq!(
            resp.headers
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some(ERROR_SOURCE_UPSTREAM)
        );
        mock.assert();
    }

    #[tokio::test]
    async fn unsupported_method_is_validation_error() {
        let h = Harness::new().await;
        // Add a catch-all route matching CONNECT so resolution succeeds and
        // the forward-time method check rejects it with 400.
        let mut route = Harness::route();
        route.id = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_0002);
        route.match_.http.as_mut().unwrap().methods = vec![crate::domain::models::HttpMethod::Connect];
        h.store.clone().routes().insert(route).await.unwrap();

        let err = h
            .proxy()
            .handle(&ctx(), Harness::request(Method::CONNECT, "/v1"), "vendor")
            .await
            .unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[tokio::test]
    async fn request_timeout_maps_to_request_timeout() {
        let h = Harness::new().await;
        h.server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1");
            then.delay(std::time::Duration::from_secs(5)).status(200);
        });
        // A 200ms-timeout client against a 5s-delayed mock → 504.
        let proxy = h.proxy_with_timeout(0);
        let err = proxy
            .handle(&ctx(), Harness::request(Method::GET, "/v1"), "vendor")
            .await
            .unwrap_err();
        assert!(matches!(err, DataPlaneError::RequestTimeout { .. }));
    }

    #[tokio::test]
    async fn transport_failure_maps_to_503() {
        let h = Harness::new().await;
        // Point the upstream at a closed port (socket refused).
        let mut upstream = h.upstream();
        upstream.server.endpoints[0].host = "127.0.0.1".to_owned();
        upstream.server.endpoints[0].port = Some(1);
        h.store.clone().upstreams().replace(TENANT, upstream).await.unwrap();

        let err = h
            .proxy()
            .handle(&ctx(), Harness::request(Method::GET, "/v1"), "vendor")
            .await
            .unwrap_err();
        assert!(matches!(err, DataPlaneError::LinkUnavailable { .. }));
    }

    #[test]
    fn validate_inbound_checks_framing_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_static("5"),
        );
        assert!(validate_inbound(&headers, 5).is_ok());
        assert!(validate_inbound(&headers, 3).is_err());

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_static("abc"),
        );
        assert!(validate_inbound(&headers, 0).is_err());

        let mut headers = HeaderMap::new();
        headers.insert(
            TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        assert!(validate_inbound(&headers, 0).is_ok());
        headers.insert(
            TRANSFER_ENCODING,
            HeaderValue::from_static("gzip"),
        );
        assert!(validate_inbound(&headers, 0).is_err());
    }

    #[test]
    fn upstream_authority_omits_default_port() {
        let e = |scheme: EndpointScheme, host: &str, port: Option<u16>| Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        };
        assert_eq!(
            upstream_authority(&e(EndpointScheme::Https, "api.example.com", None)),
            "api.example.com"
        );
        assert_eq!(
            upstream_authority(&e(EndpointScheme::Http, "127.0.0.1", Some(8080))),
            "127.0.0.1:8080"
        );
    }


#[tokio::test]
async fn handle_future_is_send() {
    let h = Harness::new().await;
    let proxy = h.proxy();
    let ctx = toolkit_security::SecurityContext::anonymous();
    let req = ProxyRequest {
        method: Method::GET,
        path: "/v1".to_owned(),
        query: None,
        headers: HeaderMap::new(),
        body: bytes::Bytes::new(),
    };
    // `tokio::spawn` requires the future to be `Send`; this fails to
    // compile if the pipeline holds a non-`Send` type across an await
    // (which would also break axum `Handler` registration and therefore
    // route mounting).
    let handle = tokio::spawn(async move {
        drop(proxy.handle(&ctx, req, "vendor").await);
    });
    drop(handle.await);
}
}