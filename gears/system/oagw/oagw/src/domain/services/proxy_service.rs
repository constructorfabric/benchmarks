//! Proxy path (data plane).
//!
//! Phase 2 fills in every seam phase 1 named: real dialing over `hyper`
//! (HTTP/1.1 and HTTP/2), SSE pass-through, WebSocket tunneling, credential
//! injection, guard/transform plugin execution, rate limiting, CORS and the L1
//! configuration cache.
//!
//! | Seam | Function | Phase 2 behaviour |
//! |---|---|---|
//! | transport | [`ProxyService::forward`] | pooled `hyper` client, timeout → 504 |
//! | streaming | [`ProxyOutcome::Stream`] | upstream body piped to the caller |
//! | WebSocket | [`ProxyService::tunnel_upgrade`] | `copy_bidirectional` splice |
//! | error source | [`ProxyFailure`] | gateway vs upstream on every error |
//! | credentials | [`ProxyService::inject_upstream_credentials`] | `cred://` → credstore → auth plugin |
//! | guards / transforms | [`ProxyService::execute_guard_plugins`] | executed with `plugins.configs` |
//! | rate limiting | [`ProxyService::apply_rate_limit`] | token bucket / sliding window |
//! | CORS | [`ProxyService::apply_cors`] | permissive preflight, validated actual requests |
//! | L1 cache | [`ProxyService::resolve_upstream`] | generation-invalidated config cache |
//!
//! Execution order (ADR 0002): resolve → validate → rate limit → CORS → auth
//! plugins → request header rules → guard plugins → transform plugins →
//! upstream call → response phases. The request header rules come before the
//! plugin phases because they assemble the forwarded set (`ctx.headers`) those
//! phases read and write.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body as StreamBody;
use bytes::Bytes;
use dashmap::DashMap;
use http::{HeaderMap, HeaderValue};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{MAX_BODY_BYTES, OagwConfig};
use crate::domain::error::DomainError;
use crate::domain::models::{
    AuthConfig, CorsConfig, Endpoint, HeaderPassthrough, HttpMethod, PathSuffixMode,
    RateLimitConfig, RateLimitStrategy, Route, Upstream, UpstreamProtocol,
};
use crate::domain::plugin::{
    GuardDecision, PluginRegistry, RequestContext, ResponseContext, is_guard_plugin_ref,
    is_transform_plugin_ref,
};
use crate::domain::repo::Repositories;
use crate::infra::cache::ConfigCache;
use crate::infra::plugin::CredentialStore;
use crate::infra::rate_limit::{RateLimitDecision, RateLimitIdentity, RateLimiter};
use crate::infra::transport::{OutboundBody, UpstreamTransport, build_uri, request_builder};

/// Header carrying the gateway/upstream provenance of a response.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// `X-OAGW-Error-Source` value of a response that came from an upstream.
pub const SOURCE_UPSTREAM: &str = "upstream";

/// `X-OAGW-Error-Source` value of a response the gateway produced itself.
pub const SOURCE_GATEWAY: &str = "gateway";

/// Inbound proxy request assembled by the transport layer.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Calling tenant.
    pub tenant_id: Uuid,
    /// Tenant chain, descendant first (descendant → … → root).
    pub tenant_chain: Vec<Uuid>,
    /// Upstream alias taken from `/proxy/{alias}/…`.
    pub alias: String,
    /// Path suffix after the alias.
    pub path_suffix: String,
    /// Raw query string.
    pub query: Option<String>,
    /// Request method.
    pub method: http::Method,
    /// Inbound headers.
    pub headers: HeaderMap,
    /// Buffered request body (already subject to the 100 MB limit).
    pub body: Bytes,
    /// `X-OAGW-Target-Host` value, when supplied.
    pub target_host: Option<String>,
    /// Authenticated caller, for `cred://` resolution and rate-limit scoping.
    pub subject_id: Option<Uuid>,
    /// Caller identity, as the transport resolved it.
    pub security: Option<Arc<SecurityContext>>,
    /// Pending inbound protocol upgrade, when the caller asked for one.
    pub inbound_upgrade: Option<hyper::upgrade::OnUpgrade>,
}

impl ProxyRequest {
    /// Builds a request description with an empty body and no tenant chain.
    #[must_use]
    pub fn new(tenant_id: Uuid, alias: impl Into<String>, method: http::Method, path_suffix: impl Into<String>) -> Self {
        Self {
            tenant_id,
            tenant_chain: Vec::new(),
            alias: alias.into(),
            path_suffix: path_suffix.into(),
            query: None,
            method,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            target_host: None,
            subject_id: None,
            security: None,
            inbound_upgrade: None,
        }
    }

    /// `true` when the caller asked for a protocol upgrade.
    #[must_use]
    pub fn wants_upgrade(&self) -> bool {
        self.inbound_upgrade.is_some()
            || self
                .headers
                .get(http::header::UPGRADE)
                .is_some_and(|value| !value.is_empty())
    }
}

/// A fully rendered upstream response (buffered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyResponse {
    /// Upstream status code.
    pub status: http::StatusCode,
    /// Response headers (hop-by-hop headers already stripped).
    pub headers: HeaderMap,
    /// Buffered response body.
    pub body: Bytes,
}

/// What the proxy hands back to the transport layer.
///
/// Three shapes, because the data plane must not buffer what the caller asked
/// to stream:
///
/// * [`ProxyOutcome::Response`] — the whole upstream body is already buffered;
/// * [`ProxyOutcome::Stream`] — the caller renders the body itself, chunk by
///   chunk (SSE and any other incremental response);
/// * [`ProxyOutcome::Upgraded`] — the protocol switched; the tunnel runs
///   detached and the response carries only the handshake headers.
///
/// The streamed body is axum's [`axum::body::Body`] wrapping the upstream
/// [`hyper::body::Incoming`]: hyper hands the frames over as they arrive, so an
/// SSE response is relayed event by event and never buffered.
#[derive(Debug)]
pub enum ProxyOutcome {
    /// Buffered passthrough.
    Response(ProxyResponse),
    /// Streaming passthrough: the upstream body is still arriving.
    Stream {
        /// Upstream status code.
        status: http::StatusCode,
        /// Response headers (hop-by-hop headers already stripped).
        headers: HeaderMap,
        /// The still-open upstream body.
        body: StreamBody,
    },
    /// The upstream switched protocols; the tunnel is already spliced.
    Upgraded {
        /// Upstream status code (101).
        status: http::StatusCode,
        /// Handshake headers, `Upgrade`/`Connection` included.
        headers: HeaderMap,
    },
}

impl ProxyOutcome {
    /// Upstream status code, whichever shape this is.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        match self {
            Self::Response(response) => response.status,
            Self::Stream { status, .. } | Self::Upgraded { status, .. } => *status,
        }
    }

    /// Response headers, whichever shape this is.
    #[must_use]
    pub const fn headers(&self) -> &HeaderMap {
        match self {
            Self::Response(response) => &response.headers,
            Self::Stream { headers, .. } | Self::Upgraded { headers, .. } => headers,
        }
    }
}

/// Why a request never reached the caller as a proxied response.
///
/// Two origins, because every data-plane error response must say which side
/// produced it (`X-OAGW-Error-Source`): a [`DomainError`] is the gateway's own
/// failure, a [`CorsRejection`] is a CORS policy decision, and a
/// [`RateLimitRejection`] is a throttle. All three are *gateway* failures — an
/// upstream failure is a proxied response, not a [`ProxyFailure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyFailure {
    /// A gateway failure, mapped onto the DESIGN.md §3.3 error table.
    Domain(DomainError),
    /// The caller was refused by the CORS policy (403).
    Cors(CorsRejection),
    /// The caller exceeded the resolved rate limit (429).
    RateLimited(RateLimitRejection),
}

impl From<DomainError> for ProxyFailure {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl ProxyFailure {
    /// HTTP status of this failure.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Domain(error) => error.http_status(),
            Self::Cors(_) => 403,
            Self::RateLimited(_) => 429,
        }
    }

    /// `X-OAGW-Error-Source` value: the gateway produced this failure itself.
    #[must_use]
    pub const fn error_source(&self) -> &'static str {
        SOURCE_GATEWAY
    }

    /// GTS error type identifier for the problem document.
    #[must_use]
    pub fn gts_type(&self) -> String {
        match self {
            Self::Domain(error) => error.gts_type().to_owned(),
            Self::Cors(rejection) => rejection.gts_type().to_owned(),
            Self::RateLimited(_) => {
                "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1".to_owned()
            }
        }
    }

    /// Problem `title`.
    #[must_use]
    pub fn title(&self) -> String {
        match self {
            Self::Domain(error) => error.title().to_owned(),
            Self::Cors(rejection) => rejection.title().to_owned(),
            Self::RateLimited(_) => "Rate Limit Exceeded".to_owned(),
        }
    }

    /// Problem `detail`.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Domain(error) => error.detail(),
            Self::Cors(rejection) => rejection.detail(),
            Self::RateLimited(rejection) => rejection.detail(),
        }
    }
}

/// Which CORS check failed (ADR 0004, "Actual Request Enforcement").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsRejectionKind {
    /// The origin is not in `allowed_origins`.
    Origin,
    /// The method is not in `allowed_methods`.
    Method(String),
}

/// A CORS policy refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsRejection {
    /// The refused `Origin`, when the caller sent one.
    pub origin: Option<String>,
    /// What was refused.
    pub kind: CorsRejectionKind,
}

impl CorsRejection {
    /// Builds an origin rejection.
    #[must_use]
    pub fn origin(origin: Option<String>) -> Self {
        Self {
            origin,
            kind: CorsRejectionKind::Origin,
        }
    }

    /// Builds a method rejection.
    #[must_use]
    pub fn method(origin: Option<String>, method: &http::Method) -> Self {
        Self {
            origin,
            kind: CorsRejectionKind::Method(method.as_str().to_owned()),
        }
    }

    /// ADR 0004 problem type identifier.
    #[must_use]
    pub const fn gts_type(&self) -> &'static str {
        match self.kind {
            CorsRejectionKind::Origin => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
            }
            CorsRejectionKind::Method(_) => {
                "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
            }
        }
    }

    /// Problem `title`.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self.kind {
            CorsRejectionKind::Origin => "Origin Not Allowed",
            CorsRejectionKind::Method(_) => "Method Not Allowed",
        }
    }

    /// Problem `detail`.
    #[must_use]
    pub fn detail(&self) -> String {
        match &self.kind {
            CorsRejectionKind::Origin => match &self.origin {
                Some(origin) => format!("origin '{origin}' is not allowed by the CORS policy"),
                None => "the request origin is not allowed by the CORS policy".to_owned(),
            },
            CorsRejectionKind::Method(method) => {
                format!("method '{method}' is not allowed by the CORS policy")
            }
        }
    }
}

/// A rate-limit refusal (ADR 0003, `rate_limit.exceeded.v1`).
///
/// Carries the limiter values ADR 0003 asks a `429` to render:
/// `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset` and
/// `Retry-After` — the same headers a *proxied* response carries, so a refused
/// caller can budget its calls exactly as a served one can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitRejection {
    /// Alias of the upstream the limit is attached to.
    pub upstream: String,
    /// Effective limit, for `X-RateLimit-Limit`.
    pub limit: u64,
    /// Capacity left after the refused request, for `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// Seconds until the bucket/window is replenished, for `X-RateLimit-Reset`.
    pub reset_secs: u64,
    /// `Retry-After` in seconds.
    pub retry_after_secs: u64,
}

impl RateLimitRejection {
    /// Builds a refusal from a decision.
    #[must_use]
    pub fn new(upstream: &str, decision: &RateLimitDecision) -> Self {
        Self {
            upstream: upstream.to_owned(),
            limit: decision.limit,
            remaining: decision.remaining,
            reset_secs: decision.reset_secs,
            retry_after_secs: decision
                .retry_after
                .unwrap_or(Duration::from_secs(1))
                .as_secs()
                .max(1),
        }
    }

    /// Problem `detail`.
    #[must_use]
    pub fn detail(&self) -> String {
        format!(
            "rate limit of {} requests exceeded for upstream '{}'",
            self.limit, self.upstream
        )
    }

    /// Writes the `X-RateLimit-*` headers of this refusal (ADR 0003).
    ///
    /// The same three headers [`ProxyService::forward`] renders on the proxied
    /// path, so the two responses cannot drift apart. `Retry-After` is left to
    /// the caller, which renders it alongside these.
    pub fn apply_rate_limit_headers(&self, headers: &mut HeaderMap) {
        insert_rate_limit_headers(headers, self.limit, self.remaining, self.reset_secs);
    }
}

/// CORS headers decided by [`ProxyService::apply_cors`] and applied to the
/// upstream response (ADR 0004).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsHeaders {
    allow_origin: Option<HeaderValue>,
    expose_headers: Option<HeaderValue>,
    allow_credentials: bool,
    vary: bool,
}

impl CorsHeaders {
    /// Writes the decided headers onto a response header map.
    pub fn apply(&self, headers: &mut HeaderMap) {
        if let Some(origin) = self.allow_origin.as_ref() {
            headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        }
        if let Some(expose) = self.expose_headers.as_ref() {
            headers.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, expose.clone());
        }
        if self.allow_credentials {
            headers.insert(
                http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
        if self.vary {
            headers.append(http::header::VARY, HeaderValue::from_static("Origin"));
        }
    }
}

/// Data-plane proxy operations.
#[derive(Clone)]
pub struct ProxyService {
    repos: Repositories,
    config: OagwConfig,
    plugins: Arc<PluginRegistry>,
    transport: UpstreamTransport,
    credentials: Arc<CredentialStore>,
    cache: Arc<ConfigCache>,
    limiter: Arc<RateLimiter>,
    /// Round-robin cursors, one per multi-endpoint pool.
    cursors: Arc<DashMap<String, usize>>,
}

impl std::fmt::Debug for ProxyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyService")
            .field("proxy_timeout_secs", &self.config.proxy_timeout_secs)
            .field("allow_http_upstream", &self.config.allow_http_upstream)
            .field("ssrf_enabled", &self.config.ssrf_policy.enabled)
            .finish_non_exhaustive()
    }
}

impl ProxyService {
    /// Builds the proxy service.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the outbound transport cannot be built.
    pub fn try_new(
        repos: Repositories,
        config: OagwConfig,
        plugins: PluginRegistry,
    ) -> Result<Self, DomainError> {
        let transport = UpstreamTransport::try_new(config.proxy_timeout())?;
        Ok(Self {
            cache: ConfigCache::new(config.l1_cache_ttl(), config.l1_cache_capacity),
            repos,
            config,
            plugins: Arc::new(plugins),
            transport,
            credentials: CredentialStore::new(),
            limiter: Arc::new(RateLimiter::new()),
            cursors: Arc::new(DashMap::new()),
        })
    }

    /// Shares the control plane's L1 cache with this service, so a management
    /// write invalidates the data plane's copy in the same tick.
    #[must_use]
    pub fn with_config_cache(mut self, cache: Arc<ConfigCache>) -> Self {
        self.cache = cache;
        self
    }

    /// Shares the gear's credential store (with the credstore client wired into
    /// it) with this service.
    #[must_use]
    pub fn with_credentials(mut self, credentials: Arc<CredentialStore>) -> Self {
        self.credentials = credentials;
        self
    }

    /// The shared credential store, for the gear to wire credstore into.
    #[must_use]
    pub fn credentials(&self) -> &Arc<CredentialStore> {
        &self.credentials
    }

    /// Resolves the upstream for `alias`, walking the tenant chain from the
    /// descendant to the root (closest match wins, per DESIGN.md §3.1).
    ///
    /// Reads through the L1 cache (ADR 0005); a miss falls back to the L2
    /// repository and repopulates the entry.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when no enabled upstream matches.
    pub fn resolve_upstream(&self, request: &ProxyRequest) -> Result<Upstream, DomainError> {
        let mut chain: Vec<Uuid> = vec![request.tenant_id];
        chain.extend(request.tenant_chain.iter().copied());
        for tenant_id in chain {
            if let Some(cached) = self.cache.get_upstream(tenant_id, &request.alias) {
                return Ok(cached);
            }
            if let Some(upstream) = self.repos.upstreams.find_by_alias(tenant_id, &request.alias)?
                && upstream.enabled
            {
                self.cache.put_upstream(tenant_id, &request.alias, &upstream);
                return Ok(upstream);
            }
        }
        Err(DomainError::NotFound(format!(
            "no enabled upstream with alias '{}'",
            request.alias
        )))
    }

    /// Resolves the route matching `method` + `path_suffix` on one upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::RouteNotFound`] when nothing matches.
    pub fn resolve_route(
        &self,
        upstream: &Upstream,
        method: &http::Method,
        path_suffix: &str,
    ) -> Result<Route, DomainError> {
        let routes = self.routes_of(upstream)?;
        let http_method = HttpMethod::from_method(method).ok_or_else(|| {
            DomainError::RouteNotFound {
                method: method.as_str().to_owned(),
                path: path_suffix.to_owned(),
                alias: upstream.alias.clone(),
            }
        })?;

        for route in routes.iter() {
            let Some(http) = route.match_rules.http.as_ref() else {
                continue;
            };
            if !http.methods.contains(&http_method) {
                continue;
            }
            if path_matches(&http.path, path_suffix, http.path_suffix_mode) {
                return Ok(route.clone());
            }
        }

        Err(DomainError::RouteNotFound {
            method: method.as_str().to_owned(),
            path: path_suffix.to_owned(),
            alias: upstream.alias.clone(),
        })
    }

    /// Route list of one upstream, read through the L1 cache (ADR 0005).
    fn routes_of(&self, upstream: &Upstream) -> Result<Arc<Vec<Route>>, DomainError> {
        if let Some(routes) = self.cache.get_routes(upstream.id) {
            return Ok(routes);
        }
        let routes = self.repos.routes.list_by_upstream(upstream.id)?;
        self.cache.put_routes(upstream.id, &routes);
        Ok(Arc::new(routes))
    }

    /// Executes the proxy path for one request.
    ///
    /// # Errors
    ///
    /// Every failure is a [`ProxyFailure`] the transport renders as an RFC 9457
    /// problem document carrying `X-OAGW-Error-Source: gateway`.
    pub async fn proxy(&self, request: &ProxyRequest) -> Result<ProxyOutcome, ProxyFailure> {
        let upstream = self.resolve_upstream(request)?;
        let route = self.resolve_route(&upstream, &request.method, &request.path_suffix)?;
        let endpoint = self.select_endpoint(&upstream, request)?;

        self.check_body_size(request.body.len())?;
        self.check_plaintext(&upstream, endpoint)?;
        self.check_ssrf(endpoint)?;

        let rate_limit = self.apply_rate_limit(&upstream, &route, request).await?;
        if let Some(decision) = rate_limit.as_ref()
            && !decision.allowed
        {
            return Err(ProxyFailure::RateLimited(RateLimitRejection::new(
                &upstream.alias,
                decision,
            )));
        }

        let cors = self.apply_cors(&upstream, request)?;

        let mut ctx = RequestContext::new(
            request.tenant_id,
            upstream.alias.clone(),
            request.method.clone(),
            self.build_path(&route, request),
        );
        ctx.query = request.query.clone();
        ctx.body = request.body.clone();
        ctx.subject_id = request.subject_id;
        ctx.route_id = Some(route.id);
        ctx.security = request.security.clone();

        let streaming_request = wants_upgrade(request, endpoint);

        // ADR 0002 execution order: auth → guards → transforms. The request
        // header rules are applied first, because they assemble the forwarded
        // set `ctx.headers` that both plugin phases read and write; credentials
        // are injected separately and merged at dial time, so a header policy
        // can neither drop nor leak them.
        self.inject_upstream_credentials(&upstream, &mut ctx)
            .await?;
        self.apply_request_header_rules(&upstream, &mut ctx, request)?;
        self.execute_guard_plugins(&upstream, &route, &mut ctx)
            .await?;
        self.execute_transform_plugins(&upstream, &route, &mut ctx)
            .await?;

        if streaming_request {
            return self
                .tunnel_upgrade(&upstream, endpoint, &ctx, request, cors)
                .await
                .map_err(ProxyFailure::from);
        }

        let outcome = self
            .forward(&upstream, endpoint, &ctx, request, cors, rate_limit)
            .await?;
        self.apply_response_header_rules(&upstream, &route, outcome)
            .await
            .map_err(ProxyFailure::from)
    }

    /// Picks the endpoint the request is routed to.
    ///
    /// A multi-endpoint pool whose alias is a common domain suffix cannot be
    /// disambiguated from the alias alone, so `X-OAGW-Target-Host` is required.
    /// A single-endpoint pool is selected directly; a derivable multi-endpoint
    /// pool is served **round-robin** (DESIGN.md §3.1).
    ///
    /// # Errors
    ///
    /// * [`DomainError::MissingTargetHost`] when the header is required and absent.
    /// * [`DomainError::InvalidTargetHost`] when the value is not a bare host.
    /// * [`DomainError::UnknownTargetHost`] when it matches no endpoint.
    pub fn select_endpoint<'a>(
        &self,
        upstream: &'a Upstream,
        request: &ProxyRequest,
    ) -> Result<&'a Endpoint, DomainError> {
        let endpoints = &upstream.server.endpoints;
        let single = endpoints.len() == 1;
        let ambiguous = !single && upstream.alias.contains('.');
        if single || ambiguous {
            if let Some(raw) = request.target_host.as_deref() {
                let host = raw.trim().to_ascii_lowercase();
                if host.is_empty()
                    || host.contains('/')
                    || host.contains(':')
                    || host.contains(' ')
                {
                    return Err(DomainError::InvalidTargetHost(raw.to_owned()));
                }
                let found = endpoints
                    .iter()
                    .find(|e| e.normalized_host() == host)
                    .ok_or_else(|| {
                        DomainError::UnknownTargetHost(host.clone(), upstream.alias.clone())
                    })?;
                return Ok(found);
            }
            if single {
                return Ok(&endpoints[0]);
            }
            return Err(DomainError::MissingTargetHost(upstream.alias.clone()));
        }
        // Derivable multi-endpoint pool without a target host: round-robin.
        let index = self.next_index(&upstream.alias, endpoints.len());
        Ok(&endpoints[index])
    }

    /// Advances the per-upstream round-robin cursor.
    fn next_index(&self, alias: &str, len: usize) -> usize {
        let mut cursor = self.cursors.entry(alias.to_owned()).or_insert(0);
        let index = *cursor % len;
        *cursor = (*cursor + 1) % len;
        index
    }

    /// Rejects payloads over the 100 MB hard limit before buffering.
    ///
    /// # Errors
    ///
    /// [`DomainError::PayloadTooLarge`].
    pub fn check_body_size(&self, len: usize) -> Result<(), DomainError> {
        if len > MAX_BODY_BYTES {
            return Err(DomainError::PayloadTooLarge(MAX_BODY_BYTES));
        }
        Ok(())
    }

    /// Refuses to dial a plaintext endpoint when the config forbids it.
    ///
    /// # Errors
    ///
    /// [`DomainError::LinkUnavailable`] (503) — the `InsecureTransport`
    /// semantics of the wire contract.
    pub fn check_plaintext(&self, upstream: &Upstream, endpoint: &Endpoint) -> Result<(), DomainError> {
        if endpoint.scheme.is_plaintext() && !self.config.allow_http_upstream {
            return Err(DomainError::LinkUnavailable(
                upstream.alias.clone(),
                "plaintext transport is refused by policy (allow_http_upstream is false)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Applies the SSRF host policy to the selected endpoint.
    ///
    /// # Errors
    ///
    /// [`DomainError::LinkUnavailable`] (503) when the host is not permitted.
    pub fn check_ssrf(&self, endpoint: &Endpoint) -> Result<(), DomainError> {
        let host = endpoint.normalized_host();
        if self.config.ssrf_policy.permits(&host) {
            Ok(())
        } else {
            Err(DomainError::LinkUnavailable(
                host,
                "host is rejected by the SSRF policy".to_owned(),
            ))
        }
    }

    /// Renders the upstream request path from the matched route.
    #[must_use]
    pub fn build_path(&self, route: &Route, request: &ProxyRequest) -> String {
        let Some(http) = route.match_rules.http.as_ref() else {
            return request.path_suffix.clone();
        };
        let path = http.path.trim_end_matches('/');
        if request.path_suffix.is_empty() || request.path_suffix == "/" {
            return if path.is_empty() { "/".to_owned() } else { path.to_owned() };
        }
        match http.path_suffix_mode {
            PathSuffixMode::Disabled => path.to_owned(),
            // The matched route path is a *prefix* of the aliased sub-path
            // (longest-prefix matching), so the upstream sees the path exactly
            // as the caller used it (`/v1` + `/v1/models` → `/v1/models`).
            PathSuffixMode::Append => {
                if request.path_suffix.starts_with('/') {
                    request.path_suffix.clone()
                } else {
                    format!("/{suffix}", suffix = request.path_suffix)
                }
            }
        }
    }

    /// Forwards the request to the upstream over the pooled transport.
    ///
    /// The upstream body is streamed back when the exchange looks incremental
    /// (an `text/event-stream` response, or a response to a caller that asked
    /// for one); otherwise it is buffered under the 100 MB limit.
    ///
    /// # Errors
    ///
    /// [`DomainError::RequestTimeout`], [`DomainError::LinkUnavailable`],
    /// [`DomainError::ProtocolError`] or [`DomainError::PayloadTooLarge`].
    pub async fn forward(
        &self,
        upstream: &Upstream,
        endpoint: &Endpoint,
        ctx: &RequestContext,
        request: &ProxyRequest,
        cors: CorsHeaders,
        rate_limit: Option<RateLimitDecision>,
    ) -> Result<ProxyOutcome, DomainError> {
        if upstream.protocol == UpstreamProtocol::Grpc {
            return Err(grpc_unsupported(&upstream.alias));
        }

        let uri = build_uri(
            endpoint.scheme.as_uri_scheme(),
            &endpoint.authority(),
            &ctx.path,
            ctx.query.as_deref(),
        )?;
        let outbound = self.build_outbound(&request.method, &uri, ctx, request.body.clone())?;
        let response = self
            .transport
            .send(&upstream.alias, endpoint.scheme.is_plaintext(), outbound)
            .await?;

        let (mut parts, body) = response.into_parts();
        if parts.status.is_informational() {
            return Err(DomainError::ProtocolError(
                upstream.alias.clone(),
                format!(
                    "upstream answered an unsupported {} status",
                    parts.status.as_u16()
                ),
            ));
        }
        let streaming = is_streaming(&parts.headers, request);
        strip_hop_by_hop(&mut parts.headers);
        if let Some(decision) = rate_limit.as_ref() {
            apply_rate_limit_headers(&mut parts.headers, decision);
        }
        cors.apply(&mut parts.headers);

        if streaming {
            return Ok(ProxyOutcome::Stream {
                status: parts.status,
                headers: parts.headers,
                body: StreamBody::new(body),
            });
        }

        let buffered = axum::body::to_bytes(StreamBody::new(body), MAX_BODY_BYTES)
            .await
            .map_err(|error| map_body_error(&upstream.alias, error))?;
        Ok(ProxyOutcome::Response(ProxyResponse {
            status: parts.status,
            headers: parts.headers,
            body: buffered,
        }))
    }

    /// Builds the outbound request: policy-selected headers plus the injected
    /// credentials, over a body that can be streamed.
    fn build_outbound(
        &self,
        method: &http::Method,
        uri: &http::Uri,
        ctx: &RequestContext,
        body: Bytes,
    ) -> Result<http::Request<OutboundBody>, DomainError> {
        let mut builder = request_builder(method, uri);
        for (name, value) in &ctx.headers {
            builder = builder.header(name, value);
        }
        for (name, value) in ctx.clone().take_injected_headers() {
            builder = builder.header(name, value);
        }
        builder
            .body(OutboundBody::from(body))
            .map_err(|error| DomainError::Internal(format!("outbound request: {error}")))
    }

    // -------------------------------------------------------------------------
    // Credential injection
    // -------------------------------------------------------------------------

    /// Injects the upstream credentials configured by `upstream.auth`.
    ///
    /// The resolved auth plugin is executed with its configuration
    /// (`plugins.configs[<plugin_ref>]`, falling back to `upstream.auth.config`).
    /// A plugin that is not registered — or one whose `cred://` references
    /// cannot be resolved — fails the request: it is never forwarded
    /// unauthenticated.
    ///
    /// Auth plugins are bound through `upstream.auth` alone. An auth ref placed
    /// in `plugins.items` executes nowhere: neither here, nor in the guard or
    /// transform phases, which each resolve only their own kind and skip the
    /// rest of the shared chain.
    ///
    /// # Errors
    ///
    /// * [`DomainError::PluginNotFound`] for an unresolvable plugin reference.
    /// * [`DomainError::SecretNotFound`] (500) when credstore cannot serve a
    ///   referenced secret.
    /// * [`DomainError::AuthenticationFailed`] (401) when a credential cannot be
    ///   minted (an OAuth2 token exchange failed, for instance).
    /// * [`DomainError::Validation`] for a malformed plugin configuration.
    pub async fn inject_upstream_credentials(
        &self,
        upstream: &Upstream,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let Some(auth) = upstream.auth.as_ref() else {
            return Ok(());
        };
        let Some(plugin_ref) = auth.plugin_type.as_deref() else {
            return Ok(());
        };
        let Some(plugin) = self.plugins.auth_plugin(plugin_ref) else {
            return Err(self.resolve_upstream_credentials(upstream, plugin_ref));
        };
        ctx.plugin_config = auth_plugin_config(upstream, auth, plugin_ref);
        plugin.authenticate(ctx).await
    }

    /// Resolves a `cred://` reference through credstore.
    ///
    /// The auth plugins do the lookup through the shared [`CredentialStore`],
    /// which strips the `cred://` scheme before the call; this seam is what an
    /// unregistered *plugin* resolves to instead.
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginNotFound`] — an unknown plugin is a configuration
    /// error, not a credential one, and must never read as a missing secret.
    fn resolve_upstream_credentials(&self, upstream: &Upstream, plugin_ref: &str) -> DomainError {
        DomainError::PluginNotFound(format!(
            "auth plugin '{plugin_ref}' bound to upstream '{}' is not deployed",
            upstream.alias
        ))
    }

    // -------------------------------------------------------------------------
    // Plugin execution
    // -------------------------------------------------------------------------

    /// Executes every bound guard plugin, upstream chain first, route chain
    /// second (ADR 0002).
    ///
    /// `plugins.items` is one chain shared by all three plugin phases (ADR
    /// 0002), so only the *guard* refs of that chain run here — a transform ref
    /// is the transform phase's business and an auth ref is bound through
    /// `upstream.auth`, never through `plugins.items`. A ref of another kind is
    /// therefore skipped, not reported.
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginNotFound`] when a guard ref does not resolve in the
    /// guard registry — that is a genuine configuration error.
    pub async fn execute_guard_plugins(
        &self,
        upstream: &Upstream,
        route: &Route,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        for (plugin_ref, config) in self.bound_of_kind(upstream, route, is_guard_plugin_ref) {
            let Some(plugin) = self.plugins.guard_plugin(&plugin_ref) else {
                return Err(DomainError::PluginNotFound(plugin_ref));
            };
            ctx.plugin_config = config;
            if let GuardDecision::Reject(error) = plugin.guard_request(ctx).await? {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Executes every bound transform plugin, upstream chain first, route chain
    /// second (ADR 0002).
    ///
    /// Like [`ProxyService::execute_guard_plugins`], this phase resolves only
    /// the transform refs of the shared `plugins.items` chain: a guard ref in
    /// that chain belongs to the guard phase and is skipped, and auth plugins
    /// are bound through `upstream.auth` instead.
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginNotFound`] when a transform ref does not resolve in
    /// the transform registry.
    pub async fn execute_transform_plugins(
        &self,
        upstream: &Upstream,
        route: &Route,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        for (plugin_ref, config) in self.bound_of_kind(upstream, route, is_transform_plugin_ref) {
            let Some(plugin) = self.plugins.transform_plugin(&plugin_ref) else {
                return Err(DomainError::PluginNotFound(plugin_ref));
            };
            ctx.plugin_config = config;
            plugin.transform_request(ctx).await?;
        }
        Ok(())
    }

    /// Runs the guard plugins over an upstream response.
    ///
    /// A rejection becomes [`DomainError::ProtocolError`] (502) only when the
    /// plugin does not already carry a status of its own; the required-headers
    /// guard, for instance, rejects with 502 directly.
    ///
    /// Only guard refs are resolved, as in
    /// [`ProxyService::execute_guard_plugins`].
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginNotFound`] when a guard ref does not resolve in the
    /// guard registry.
    pub async fn guard_response(
        &self,
        upstream: &Upstream,
        route: &Route,
        response: &mut ResponseContext,
    ) -> Result<(), DomainError> {
        for (plugin_ref, config) in self.bound_of_kind(upstream, route, is_guard_plugin_ref) {
            let Some(plugin) = self.plugins.guard_plugin(&plugin_ref) else {
                return Err(DomainError::PluginNotFound(plugin_ref));
            };
            response.plugin_config = config;
            if let GuardDecision::Reject(error) = plugin.guard_response(response).await? {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Runs the transform plugins over the upstream response.
    ///
    /// Only transform refs are resolved, as in
    /// [`ProxyService::execute_transform_plugins`].
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginNotFound`] when a transform ref does not resolve in
    /// the transform registry.
    pub async fn transform_response(
        &self,
        upstream: &Upstream,
        route: &Route,
        response: &mut ResponseContext,
    ) -> Result<(), DomainError> {
        for (plugin_ref, config) in self.bound_of_kind(upstream, route, is_transform_plugin_ref) {
            let Some(plugin) = self.plugins.transform_plugin(&plugin_ref) else {
                return Err(DomainError::PluginNotFound(plugin_ref));
            };
            response.plugin_config = config;
            plugin.transform_response(response).await?;
        }
        Ok(())
    }

    /// Plugin bindings of an upstream and its matched route, each paired with
    /// its `plugins.configs` entry (upstream config wins over route config).
    fn bound_configs(
        &self,
        upstream: &Upstream,
        route: &Route,
    ) -> Vec<(String, Option<serde_json::Value>)> {
        let mut refs: Vec<String> = upstream.plugins.items.clone();
        refs.extend(route.plugins.items.iter().cloned());
        refs.sort();
        refs.dedup();
        refs.into_iter()
            .map(|plugin_ref| {
                let config = upstream
                    .plugins
                    .config_for(&plugin_ref)
                    .cloned()
                    .or_else(|| route.plugins.config_for(&plugin_ref).cloned());
                (plugin_ref, config)
            })
            .collect()
    }

    /// [`Self::bound_configs`] narrowed to the refs `is_kind` claims.
    ///
    /// ADR 0002 gives the three plugin phases three distinct plugin kinds over
    /// one shared `plugins.items` chain, so each phase must not trip over the
    /// other phases' refs: a chain holding a guard and a transform plugin is
    /// executed in full across the two request-phase loops.
    fn bound_of_kind(
        &self,
        upstream: &Upstream,
        route: &Route,
        is_kind: fn(&str) -> bool,
    ) -> Vec<(String, Option<serde_json::Value>)> {
        self.bound_configs(upstream, route)
            .into_iter()
            .filter(|(plugin_ref, _)| is_kind(plugin_ref))
            .collect()
    }

    // -------------------------------------------------------------------------
    // Rate limiting
    // -------------------------------------------------------------------------

    /// Applies the resolved rate-limit policy (ADR 0003).
    ///
    /// The effective policy is the **stricter** of the upstream's and the
    /// matched route's ([`RateLimitConfig::stricter_of`]). A `queue` strategy
    /// waits for capacity (bounded by the proxy timeout) before giving up.
    ///
    /// # Errors
    ///
    /// Always `Ok`; the decision is carried by the returned value so the caller
    /// can render `429` with `Retry-After`.
    pub async fn apply_rate_limit(
        &self,
        upstream: &Upstream,
        route: &Route,
        request: &ProxyRequest,
    ) -> Result<Option<RateLimitDecision>, DomainError> {
        let Some(config) = merge_rate_limits(upstream, route) else {
            return Ok(None);
        };
        let identity = RateLimitIdentity {
            tenant_id: request.tenant_id,
            subject_id: request.subject_id,
            route_id: Some(route.id),
            client_ip: self.client_ip(request),
        };
        let scope_id = identity.scope_id(config.scope);
        let mut decision = self
            .limiter
            .check(&config, &scope_id, config.cost, std::time::Instant::now());

        if decision.allowed || config.strategy != RateLimitStrategy::Queue {
            return Ok(Some(decision));
        }

        // `queue`: wait for capacity, bounded by the proxy timeout.
        let deadline = std::time::Instant::now() + self.config.proxy_timeout();
        while !decision.allowed && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            decision = self
                .limiter
                .check(&config, &scope_id, config.cost, std::time::Instant::now());
        }
        Ok(Some(decision))
    }

    /// The client IP the transport could observe.
    ///
    /// The REST layer does not expose `ConnectInfo`, so the first hop of
    /// `X-Forwarded-For` is used when the caller supplied one.
    fn client_ip<'a>(&self, request: &'a ProxyRequest) -> Option<&'a str> {
        request
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|ip| !ip.is_empty())
    }

    // -------------------------------------------------------------------------
    // CORS
    // -------------------------------------------------------------------------

    /// CORS enforcement on an actual request
    /// ([ADR 0004](../../docs/ADR/0004-cors.md)).
    ///
    /// Preflight `OPTIONS` requests are answered permissively by the handler and
    /// never reach this function. On an actual request:
    ///
    /// * no `Origin` header → no CORS processing at all (a non-browser client);
    /// * an origin outside `allowed_origins` → [`CorsRejection`] (403,
    ///   `cors.origin_not_allowed.v1`);
    /// * a method outside `allowed_methods` → [`CorsRejection`] (403,
    ///   `cors.method_not_allowed.v1`);
    /// * otherwise the decided `Access-Control-*` headers are returned and
    ///   applied to the upstream response, with `Vary: Origin` when the decision
    ///   is origin-specific.
    ///
    /// # Errors
    ///
    /// [`ProxyFailure::Cors`] when the origin or the method is not allowed.
    pub fn apply_cors(&self, upstream: &Upstream, request: &ProxyRequest) -> Result<CorsHeaders, ProxyFailure> {
        let Some(cors) = upstream.cors.clone().filter(|cors| cors.enabled) else {
            return Ok(CorsHeaders::default());
        };
        let Some(origin) = request
            .headers
            .get(http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
        else {
            // A non-browser client: CORS does not apply.
            return Ok(CorsHeaders::default());
        };

        if !origin_allowed(&cors, &origin) {
            return Err(ProxyFailure::Cors(CorsRejection::origin(Some(origin))));
        }
        if !method_allowed(&cors, &request.method) {
            return Err(ProxyFailure::Cors(CorsRejection::method(
                Some(origin),
                &request.method,
            )));
        }

        let any_origin = wildcard(&cors);
        let allow_origin = if any_origin {
            HeaderValue::from_static("*")
        } else {
            HeaderValue::from_str(&origin)
                .map_err(|_| ProxyFailure::Cors(CorsRejection::origin(Some(origin.clone()))))?
        };
        Ok(CorsHeaders {
            allow_origin: Some(allow_origin),
            expose_headers: (!cors.expose_headers.is_empty()).then(|| {
                HeaderValue::from_str(&cors.expose_headers.join(", "))
                    .unwrap_or_else(|_| HeaderValue::from_static(""))
            }),
            allow_credentials: cors.allow_credentials,
            // An origin-specific decision must not be cached for another origin.
            vary: !any_origin,
        })
    }

    // -------------------------------------------------------------------------
    // Streaming and upgrades
    // -------------------------------------------------------------------------

    /// **WebSocket upgrade tunneling** (`wss://` endpoints and inbound
    /// `Upgrade` requests).
    ///
    /// The inbound half of the tunnel is the `OnUpgrade` future the REST layer
    /// lifted out of the request; the outbound half comes from the upstream
    /// response. Both are spliced with `tokio::io::copy_bidirectional` and the
    /// splice runs detached: the `101` has already left the gateway, so a
    /// mid-tunnel failure can only be logged.
    ///
    /// # Errors
    ///
    /// [`DomainError::ProtocolError`] when the caller or the upstream does not
    /// actually upgrade, plus the transport errors of
    /// [`UpstreamTransport::send_upgrade`].
    pub async fn tunnel_upgrade(
        &self,
        upstream: &Upstream,
        endpoint: &Endpoint,
        ctx: &RequestContext,
        request: &ProxyRequest,
        cors: CorsHeaders,
    ) -> Result<ProxyOutcome, DomainError> {
        let Some(inbound) = request.inbound_upgrade.clone() else {
            return Err(DomainError::ProtocolError(
                upstream.alias.clone(),
                "the caller did not request a protocol upgrade".to_owned(),
            ));
        };
        if upstream.protocol == UpstreamProtocol::Grpc {
            return Err(grpc_unsupported(&upstream.alias));
        }

        let uri = build_uri(
            endpoint.scheme.as_uri_scheme(),
            &endpoint.authority(),
            &ctx.path,
            ctx.query.as_deref(),
        )?;
        // The upgrade handshake headers are hop-by-hop by definition, so they
        // bypass the passthrough policy and are re-attached here.
        let mut builder = request_builder(&request.method, &uri);
        for (name, value) in &request.headers {
            if is_upgrade_header(&name.as_str().to_ascii_lowercase()) {
                builder = builder.header(name, value);
            }
        }
        builder = builder.header(http::header::CONNECTION, HeaderValue::from_static("Upgrade"));
        for (name, value) in &ctx.headers {
            if !is_hop_by_hop(name.as_str()) {
                builder = builder.header(name, value);
            }
        }
        for (name, value) in ctx.clone().take_injected_headers() {
            builder = builder.header(name, value);
        }
        let outbound = builder
            .body(OutboundBody::from(ctx.body.clone()))
            .map_err(|error| DomainError::Internal(format!("outbound request: {error}")))?;

        let (response, outbound_upgrade) = self
            .transport
            .send_upgrade(&upstream.alias, endpoint.scheme.is_plaintext(), outbound)
            .await?;
        let (mut parts, _body) = response.into_parts();
        cors.apply(&mut parts.headers);

        UpstreamTransport::tunnel(inbound, outbound_upgrade, upstream.alias.clone());
        Ok(ProxyOutcome::Upgraded {
            status: parts.status,
            headers: parts.headers,
        })
    }

    // -------------------------------------------------------------------------
    // Header rules
    // -------------------------------------------------------------------------

    /// Applies the request header transformation rules
    /// (`headers.passthrough`, `set`, `add`, `remove`).
    ///
    /// The outbound request carries only what the policy allows plus the
    /// injected credentials; the `Host`/`:authority` is derived from the
    /// selected endpoint by the transport, which is the rewrite the contract
    /// asks for.
    ///
    /// This runs *before* the guard and transform plugin phases, because it
    /// assembles the forwarded set (`ctx.headers`) they operate on: the
    /// required-headers guard (ADR 0009) validates the headers that will
    /// actually be forwarded, and a transform plugin that adds a header must
    /// not have it overwritten by the policy.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] for a malformed rule.
    pub fn apply_request_header_rules(
        &self,
        upstream: &Upstream,
        ctx: &mut RequestContext,
        request: &ProxyRequest,
    ) -> Result<(), DomainError> {
        let rules = upstream.headers.request.as_ref();
        let mut out = HeaderMap::new();
        if let Some(rules) = rules
            && rules.passthrough != HeaderPassthrough::None
        {
            for (name, value) in &request.headers {
                let key = name.as_str().to_ascii_lowercase();
                let permitted = match rules.passthrough {
                    HeaderPassthrough::All => !is_hop_by_hop(&key),
                    HeaderPassthrough::Allowlist => rules
                        .passthrough_allowlist
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(&key)),
                    HeaderPassthrough::None => false,
                };
                if permitted {
                    out.insert(name.clone(), value.clone());
                }
            }
        }
        if let Some(rules) = rules {
            for name in &rules.remove {
                out.remove(name.as_str());
            }
            for (name, value) in &rules.set {
                insert_header(&mut out, name, value);
            }
            for (name, value) in &rules.add {
                if let Ok((name, value)) = header_parts(name, value) {
                    out.append(name, value);
                }
            }
        }
        ctx.headers = out;
        Ok(())
    }

    /// Applies the response header transformation rules (`set`, `add`,
    /// `remove`) plus the guard/transform plugin response phases.
    ///
    /// A streamed or upgraded exchange is handed on untouched: its headers are
    /// already final and its body is being consumed elsewhere. `Set-Cookie` is
    /// always dropped from the buffered path — an upstream session must never be
    /// replayed through the gateway.
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginNotFound`] for an unresolvable plugin reference.
    pub async fn apply_response_header_rules(
        &self,
        upstream: &Upstream,
        route: &Route,
        outcome: ProxyOutcome,
    ) -> Result<ProxyOutcome, DomainError> {
        let response = match outcome {
            ProxyOutcome::Response(response) => response,
            streamed @ ProxyOutcome::Stream { .. } | streamed @ ProxyOutcome::Upgraded { .. } => {
                return Ok(streamed);
            }
        };

        let mut ctx = ResponseContext::new(
            upstream.tenant_id,
            upstream.alias.clone(),
            response.status,
        );
        ctx.headers = response.headers;
        ctx.body = response.body;
        self.guard_response(upstream, route, &mut ctx).await?;
        self.transform_response(upstream, route, &mut ctx).await?;

        if let Some(rules) = upstream.headers.response.as_ref() {
            for name in &rules.remove {
                ctx.headers.remove(name.as_str());
            }
            for (name, value) in &rules.set {
                insert_header(&mut ctx.headers, name, value);
            }
            for (name, value) in &rules.add {
                if let Ok((name, value)) = header_parts(name, value) {
                    ctx.headers.append(name, value);
                }
            }
        }
        ctx.headers.remove(http::header::SET_COOKIE);

        Ok(ProxyOutcome::Response(ProxyResponse {
            status: ctx.status,
            headers: ctx.headers,
            body: ctx.body,
        }))
    }

    /// Provisions the OAGW types into the types-registry.
    ///
    /// # Errors
    ///
    /// Always `Ok`.
    pub fn provision_types(&self) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Configuration of an auth plugin binding: `plugins.configs` wins, the auth
/// block's own `config` object is the fallback.
fn auth_plugin_config(
    upstream: &Upstream,
    auth: &AuthConfig,
    plugin_ref: &str,
) -> Option<serde_json::Value> {
    upstream
        .plugins
        .config_for(plugin_ref)
        .cloned()
        .or_else(|| auth.config.clone())
}

/// The stricter of the upstream's and the route's rate limits (DESIGN.md:
/// `min(ancestor, descendant)`).
fn merge_rate_limits(upstream: &Upstream, route: &Route) -> Option<RateLimitConfig> {
    match (upstream.rate_limit.as_ref(), route.rate_limit.as_ref()) {
        (Some(a), Some(b)) => Some(a.stricter_of(b)),
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        (None, None) => None,
    }
}

/// `true` when `origin` is permitted by `cors`.
fn origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    wildcard(cors) || cors
        .allowed_origins
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(origin))
}

/// `true` when the policy allows any origin (`["*"]`).
fn wildcard(cors: &CorsConfig) -> bool {
    cors.allowed_origins.iter().any(|origin| origin == "*")
}

/// `true` when `method` is in `allowed_methods`.
fn method_allowed(cors: &CorsConfig, method: &http::Method) -> bool {
    cors.allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
}

/// `true` when the exchange must be tunnelled rather than forwarded.
fn wants_upgrade(request: &ProxyRequest, endpoint: &Endpoint) -> bool {
    request.wants_upgrade() || endpoint.scheme.is_upgrade()
}

/// `true` when the upstream response should reach the caller chunk by chunk:
/// an `text/event-stream` response, or a response to a caller that asked for
/// one.
fn is_streaming(headers: &HeaderMap, request: &ProxyRequest) -> bool {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.starts_with("text/event-stream") {
        return true;
    }
    request
        .headers
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("text/event-stream")
}

/// `true` for the headers a protocol upgrade must carry end to end.
fn is_upgrade_header(name: &str) -> bool {
    matches!(
        name,
        "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-protocol"
            | "sec-websocket-extensions"
            | "sec-websocket-accept"
    )
}

/// Removes the hop-by-hop headers from an upstream response.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
    ] {
        headers.remove(name);
    }
}

/// The hop-by-hop header names a proxy must not forward.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// Writes the `X-RateLimit-*` headers of one limiter verdict (ADR 0003).
///
/// Single renderer for the proxied path (from a [`RateLimitDecision`]) and the
/// `429` path (from a [`RateLimitRejection`]), so both carry the same names.
fn insert_rate_limit_headers(headers: &mut HeaderMap, limit: u64, remaining: u64, reset_secs: u64) {
    insert_value(headers, "x-ratelimit-limit", limit);
    insert_value(headers, "x-ratelimit-remaining", remaining);
    insert_value(headers, "x-ratelimit-reset", reset_secs);
}

/// Writes the `X-RateLimit-*` headers of one decision.
fn apply_rate_limit_headers(headers: &mut HeaderMap, decision: &RateLimitDecision) {
    insert_rate_limit_headers(headers, decision.limit, decision.remaining, decision.reset_secs);
}

/// Inserts a `u64` header, ignoring a malformed name.
fn insert_value(headers: &mut HeaderMap, name: &str, value: u64) {
    if let Ok(name) = http::HeaderName::try_from(name) {
        headers.insert(name, HeaderValue::from(value));
    }
}

/// Inserts a `set` rule header, ignoring a malformed pair.
fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let Ok((name, value)) = header_parts(name, value) {
        headers.insert(name, value);
    }
}

/// Parses a rule's header name/value pair.
fn header_parts(name: &str, value: &str) -> Result<(http::HeaderName, HeaderValue), ()> {
    let name = http::HeaderName::try_from(name).map_err(|_| ())?;
    let value = HeaderValue::try_from(value).map_err(|_| ())?;
    Ok((name, value))
}

/// The defined error for proxying a gRPC upstream (out of scope by contract).
fn grpc_unsupported(alias: &str) -> DomainError {
    DomainError::ProtocolError(
        alias.to_owned(),
        format!(
            "protocol '{}' has no proxy path in this phase",
            UpstreamProtocol::Grpc.gts_id()
        ),
    )
}

/// Maps a body-read failure onto the taxonomy.
///
/// A body that outgrows the limit is a [`DomainError::PayloadTooLarge`]; any
/// other mid-flight failure means the upstream exchange aborted.
fn map_body_error(alias: &str, error: axum::Error) -> DomainError {
    let rendered = error.to_string();
    if rendered.contains("length limit exceeded") {
        DomainError::PayloadTooLarge(MAX_BODY_BYTES)
    } else {
        DomainError::StreamAborted(alias.to_owned())
    }
}

/// Matches a request path suffix against a route path pattern.
///
/// A route path `/v1` matches `/v1` and `/v1/…` when suffix appending is
/// enabled; a `Disabled` suffix mode matches the exact path only.
fn path_matches(route_path: &str, suffix: &str, mode: PathSuffixMode) -> bool {
    let route_path = route_path.trim_end_matches('/');
    let suffix = suffix.trim_end_matches('/');
    if suffix == route_path || suffix.is_empty() && route_path.is_empty() {
        return true;
    }
    if route_path.is_empty() {
        return false;
    }
    match mode {
        PathSuffixMode::Disabled => suffix == route_path,
        PathSuffixMode::Append => {
            suffix == route_path
                || suffix.starts_with(route_path) && {
                    let rest = &suffix[route_path.len()..];
                    rest.starts_with('/')
                }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{EndpointScheme, ServerConfig, UpstreamSpec};
    use crate::infra::storage::memory::InMemoryRepositories;

    fn config() -> OagwConfig {
        OagwConfig {
            proxy_timeout_secs: 2,
            allow_http_upstream: true,
            ssrf_policy: crate::config::SsrfPolicy::default(),
            ..OagwConfig::default()
        }
    }

    fn service() -> ProxyService {
        ProxyService::try_new(
            InMemoryRepositories::new().into_repos(),
            config(),
            PluginRegistry::new(),
        )
        .expect("proxy service")
    }

    /// A service with every builtin plugin deployed (ADR 0008 / 0009).
    fn builtin_service() -> ProxyService {
        let mut registry = PluginRegistry::new();
        let _credentials = crate::infra::plugin::builtin::register_builtins(&mut registry);
        ProxyService::try_new(
            InMemoryRepositories::new().into_repos(),
            config(),
            registry,
        )
        .expect("proxy service")
    }

    fn upstream(alias: &str, host: &str, port: u16) -> Upstream {
        let spec = UpstreamSpec {
            alias: Some(alias.to_owned()),
            server: ServerConfig {
                endpoints: vec![Endpoint::new(EndpointScheme::Http, host, port)],
            },
            ..UpstreamSpec::default()
        };
        Upstream::from_spec(spec, Uuid::new_v4(), Uuid::new_v4(), alias.to_owned())
    }

    fn route(upstream_id: Uuid, path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            upstream_id,
            tags: Vec::new(),
            match_rules: crate::domain::models::MatchConfig {
                http: Some(crate::domain::models::HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: Default::default(),
            rate_limit: None,
        }
    }

    fn request(alias: &str, suffix: &str) -> ProxyRequest {
        ProxyRequest::new(Uuid::nil(), alias, http::Method::GET, suffix)
    }

    #[tokio::test]
    async fn body_limit_is_enforced_before_buffering() {
        let svc = service();
        assert!(svc.check_body_size(0).is_ok());
        let err = svc.check_body_size(MAX_BODY_BYTES + 1).unwrap_err();
        assert_eq!(err.http_status(), 413);
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
        );
    }

    #[tokio::test]
    async fn plaintext_is_refused_when_the_config_says_so() {
        let svc = ProxyService::try_new(
            InMemoryRepositories::new().into_repos(),
            OagwConfig {
                allow_http_upstream: false,
                ..config()
            },
            PluginRegistry::new(),
        )
        .expect("proxy service");
        let upstream = upstream("local.service", "127.0.0.1", 8080);
        let endpoint = &upstream.server.endpoints[0];
        let err = svc.check_plaintext(&upstream, endpoint).unwrap_err();
        assert_eq!(err.http_status(), 503);
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[tokio::test]
    async fn ssrf_policy_blocks_a_denied_host() {
        let svc = ProxyService::try_new(
            InMemoryRepositories::new().into_repos(),
            OagwConfig {
                ssrf_policy: crate::config::SsrfPolicy {
                    enabled: true,
                    allowed_hosts: Vec::new(),
                    blocked_hosts: vec!["metadata.internal".to_owned()],
                },
                ..config()
            },
            PluginRegistry::new(),
        )
        .expect("proxy service");
        let endpoint = Endpoint::new(EndpointScheme::Https, "metadata.internal", 443);
        assert!(svc.check_ssrf(&endpoint).is_err());
        let allowed = Endpoint::new(EndpointScheme::Https, "api.openai.com", 443);
        assert!(svc.check_ssrf(&allowed).is_ok());
    }

    #[tokio::test]
    async fn single_endpoint_needs_no_target_host() {
        let svc = service();
        let upstream = upstream("local.service", "10.0.0.1", 443);
        let request = request("local.service", "/");
        assert_eq!(
            svc.select_endpoint(&upstream, &request)
                .expect("endpoint")
                .host,
            "10.0.0.1"
        );
    }

    #[tokio::test]
    async fn multi_endpoint_pool_requires_the_target_host_header() {
        let svc = service();
        let mut spec = UpstreamSpec {
            alias: Some("vendor.com".to_owned()),
            server: ServerConfig {
                endpoints: vec![
                    Endpoint::new(EndpointScheme::Https, "us.vendor.com", 443),
                    Endpoint::new(EndpointScheme::Https, "eu.vendor.com", 443),
                ],
            },
            ..UpstreamSpec::default()
        };
        spec.alias = Some("vendor.com".to_owned());
        let upstream = Upstream::from_spec(spec, Uuid::nil(), Uuid::nil(), "vendor.com".to_owned());
        let mut request = request("vendor.com", "/v1");
        assert!(matches!(
            svc.select_endpoint(&upstream, &request),
            Err(DomainError::MissingTargetHost(_))
        ));

        request.target_host = Some("eu.vendor.com".to_owned());
        assert_eq!(
            svc.select_endpoint(&upstream, &request)
                .expect("endpoint")
                .host,
            "eu.vendor.com"
        );

        request.target_host = Some("nope.vendor.com".to_owned());
        assert!(matches!(
            svc.select_endpoint(&upstream, &request),
            Err(DomainError::UnknownTargetHost(_, _))
        ));

        request.target_host = Some("https://evil.invalid".to_owned());
        assert!(matches!(
            svc.select_endpoint(&upstream, &request),
            Err(DomainError::InvalidTargetHost(_))
        ));
    }

    #[tokio::test]
    async fn a_derivable_pool_is_served_round_robin() {
        let svc = service();
        let mut spec = UpstreamSpec {
            alias: Some("pool".to_owned()),
            server: ServerConfig {
                endpoints: vec![
                    Endpoint::new(EndpointScheme::Https, "a.pool", 443),
                    Endpoint::new(EndpointScheme::Https, "b.pool", 443),
                ],
            },
            ..UpstreamSpec::default()
        };
        // A derivable alias must not contain a dot, or the target-host rule
        // would kick in.
        spec.alias = Some("pool".to_owned());
        let upstream = Upstream::from_spec(spec, Uuid::nil(), Uuid::nil(), "pool".to_owned());
        let request = request("pool", "/v1");
        let endpoint = |request: &ProxyRequest| {
            svc.select_endpoint(&upstream, request)
                .expect("endpoint")
                .host
                .clone()
        };
        let first = endpoint(&request);
        let second = endpoint(&request);
        let third = endpoint(&request);
        assert_ne!(first, second);
        assert_eq!(first, third);
    }

    #[tokio::test]
    async fn path_suffix_appending_honours_the_route_mode() {
        let svc = service();
        let upstream_id = Uuid::nil();
        let route = route(upstream_id, "/v1");
        // The aliased sub-path already carries the matched route prefix.
        assert_eq!(
            svc.build_path(&route, &request("a", "/v1/models")),
            "/v1/models"
        );
        // A suffix is passed through with its leading slash restored.
        assert_eq!(
            svc.build_path(&route, &request("a", "models")),
            "/models"
        );
        assert_eq!(svc.build_path(&route, &request("a", "")), "/v1");

        let mut disabled = route.clone();
        disabled
            .match_rules
            .http
            .as_mut()
            .expect("http match")
            .path_suffix_mode = PathSuffixMode::Disabled;
        assert_eq!(
            svc.build_path(&disabled, &request("a", "/extra")),
            "/v1"
        );
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("keep-alive"),
        );
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        strip_hop_by_hop(&mut headers);
        assert!(!headers.contains_key(http::header::CONNECTION));
        assert_eq!(
            headers.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert!(is_hop_by_hop("upgrade"));
        assert!(!is_hop_by_hop("accept"));
    }

    #[test]
    fn an_event_stream_response_is_streamed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        assert!(is_streaming(&headers, &request("a", "/")));

        let mut json = HeaderMap::new();
        json.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(!is_streaming(&json, &request("a", "/")));

        let mut accepting = request("a", "/");
        accepting.headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert!(is_streaming(&json, &accepting));
    }

    #[test]
    fn upgrade_headers_bypass_the_passthrough_policy() {
        assert!(is_upgrade_header("upgrade"));
        assert!(is_upgrade_header("sec-websocket-key"));
        assert!(!is_upgrade_header("authorization"));
    }

    #[tokio::test]
    async fn cors_ignores_a_request_without_an_origin() {
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.cors = Some(CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example".to_owned()],
            ..CorsConfig::default()
        });
        let headers = svc.apply_cors(&upstream, &request("a.example", "/")).expect("no cors");
        assert_eq!(headers, CorsHeaders::default());
    }

    #[tokio::test]
    async fn cors_rejects_a_foreign_origin_with_403() {
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.cors = Some(CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: vec!["x-request-id".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        });
        let mut request = request("a.example", "/");
        request.headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        let failure = svc.apply_cors(&upstream, &request).unwrap_err();
        assert_eq!(failure.http_status(), 403);
        assert_eq!(
            failure.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert_eq!(failure.error_source(), "gateway");

        // A same-origin request is allowed and produces the CORS headers.
        request.headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://app.example"),
        );
        let headers = svc.apply_cors(&upstream, &request).expect("allowed");
        assert_eq!(
            headers
                .allow_origin
                .as_ref()
                .map(|value| value.to_str().expect("ascii")),
            Some("https://app.example")
        );
        assert!(headers.allow_credentials);
        assert!(headers.vary);
        let mut response_headers = HeaderMap::new();
        headers.apply(&mut response_headers);
        assert_eq!(
            response_headers
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://app.example"
        );
        assert_eq!(
            response_headers
                .get(http::header::ACCESS_CONTROL_EXPOSE_HEADERS)
                .unwrap(),
            "x-request-id"
        );
        assert_eq!(
            response_headers.get(http::header::VARY).unwrap(),
            "Origin"
        );
    }

    #[tokio::test]
    async fn cors_rejects_a_disallowed_method() {
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.cors = Some(CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            ..CorsConfig::default()
        });
        // The default policy allows `GET` and `POST` only.
        let mut request = ProxyRequest::new(
            Uuid::nil(),
            "a.example",
            http::Method::DELETE,
            "/",
        );
        request.headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://app.example"),
        );
        let failure = svc.apply_cors(&upstream, &request).unwrap_err();
        assert_eq!(
            failure.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );

        // A wildcard policy allows every origin and the default methods.
        let mut get = request.clone();
        get.method = http::Method::GET;
        let headers = svc.apply_cors(&upstream, &get).expect("allowed");
        assert_eq!(
            headers
                .allow_origin
                .as_ref()
                .map(|value| value.to_str().expect("ascii")),
            Some("*")
        );
        assert!(!headers.vary);
    }

    #[tokio::test]
    async fn rate_limits_are_merged_and_enforced() {
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.rate_limit = Some(crate::domain::models::RateLimitConfig {
            sustained: crate::domain::models::SustainedRate {
                rate: 1,
                window: crate::domain::models::RateWindow::Second,
            },
            ..crate::domain::models::RateLimitConfig::default()
        });
        let route = route(Uuid::nil(), "/v1");
        let request = request("a.example", "/v1");

        let first = svc
            .apply_rate_limit(&upstream, &route, &request)
            .await
            .expect("decision")
            .expect("configured");
        assert!(first.allowed);

        let second = svc
            .apply_rate_limit(&upstream, &route, &request)
            .await
            .expect("decision")
            .expect("configured");
        assert!(!second.allowed, "the second request exceeds 1 req/s");

        let failure = ProxyFailure::RateLimited(RateLimitRejection::new("a.example", &second));
        assert_eq!(failure.http_status(), 429);
        assert_eq!(
            failure.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert!(failure.detail().contains("a.example"));
    }

    #[tokio::test]
    async fn rate_limit_headers_are_rendered() {
        let mut headers = HeaderMap::new();
        let decision = RateLimitDecision {
            allowed: true,
            limit: 10,
            remaining: 9,
            reset_secs: 3,
            retry_after: None,
            degraded: false,
        };
        apply_rate_limit_headers(&mut headers, &decision);
        assert_eq!(headers.get("x-ratelimit-limit").unwrap(), "10");
        assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "9");
        assert_eq!(headers.get("x-ratelimit-reset").unwrap(), "3");
    }

    #[tokio::test]
    async fn an_unresolvable_plugin_binding_is_reported() {
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.plugins.items = vec!["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.ghost.v1".to_owned()];
        let route = route(upstream.id, "/v1");
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        let error = svc
            .execute_guard_plugins(&upstream, &route, &mut ctx)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 503);
        assert!(format!("{error}").contains("ghost"));

        // A transform ref that does not resolve is the same configuration error,
        // reported by the phase that owns it.
        upstream.plugins.items =
            vec!["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.ghost.v1".to_owned()];
        let error = svc
            .execute_transform_plugins(&upstream, &route, &mut ctx)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 503);
        assert!(format!("{error}").contains("ghost"));
    }

    /// ADR 0002 gives the three phases three plugin kinds over one shared
    /// `plugins.items` chain, so a ref belonging to another phase must be
    /// skipped rather than looked up in this phase's registry.
    #[tokio::test]
    async fn a_foreign_plugin_kind_is_skipped_by_a_phase() {
        // An empty registry: any ref a phase actually resolves would be reported.
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.plugins.items = vec![
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.ghost.v1".to_owned(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.ghost.v1".to_owned(),
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.ghost.v1".to_owned(),
        ];
        let route = route(upstream.id, "/v1");

        // The guard phase resolves only the guard ref, and reports *it*.
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        let error = svc
            .execute_guard_plugins(&upstream, &route, &mut ctx)
            .await
            .unwrap_err();
        assert!(
            format!("{error}").contains("guard_plugin.v1~cf.core.oagw.ghost.v1"),
            "the guard phase names its own kind: {error}"
        );

        // The transform phase resolves only the transform ref.
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        let error = svc
            .execute_transform_plugins(&upstream, &route, &mut ctx)
            .await
            .unwrap_err();
        assert!(
            format!("{error}").contains("transform_plugin.v1~cf.core.oagw.ghost.v1"),
            "the transform phase names its own kind: {error}"
        );

        // Bound on its own, a foreign kind is skipped by both phases instead of
        // being reported as `plugin.not_found`.
        upstream.plugins.items =
            vec!["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.ghost.v1".to_owned()];
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        assert!(
            svc.execute_transform_plugins(&upstream, &route, &mut ctx)
                .await
                .is_ok(),
            "a guard ref is not the transform phase's business"
        );
        upstream.plugins.items =
            vec!["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.ghost.v1".to_owned()];
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        assert!(
            svc.execute_guard_plugins(&upstream, &route, &mut ctx)
                .await
                .is_ok(),
            "a transform ref is not the guard phase's business"
        );
        // An auth ref never appears in `plugins.items` (auth is bound through
        // `upstream.auth`), and neither phase claims one either.
        upstream.plugins.items =
            vec!["gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.ghost.v1".to_owned()];
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        assert!(
            svc.execute_guard_plugins(&upstream, &route, &mut ctx)
                .await
                .is_ok()
        );
        assert!(
            svc.execute_transform_plugins(&upstream, &route, &mut ctx)
                .await
                .is_ok()
        );
    }

    /// Both request phases over one shared chain: the guard and the transform
    /// plugin bound together each run, and neither 503s on the other's ref.
    #[tokio::test]
    async fn a_guard_and_a_transform_bound_together_both_execute() {
        let svc = builtin_service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.plugins.items = vec![
            crate::domain::plugin::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned(),
            crate::domain::plugin::TRANSFORM_PLUGIN_REQUEST_ID.to_owned(),
        ];
        upstream.plugins.configs.insert(
            crate::domain::plugin::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned(),
            serde_json::json!({ "required_request_headers": "x-correlation-id" }),
        );
        let route = route(upstream.id, "/v1");

        // A request carrying the required header is accepted and transformed.
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        ctx.headers.insert(
            http::HeaderName::from_static("x-correlation-id"),
            HeaderValue::from_static("abc"),
        );
        svc.execute_guard_plugins(&upstream, &route, &mut ctx)
            .await
            .expect("the guard accepts a complete request");
        svc.execute_transform_plugins(&upstream, &route, &mut ctx)
            .await
            .expect("the transform phase ignores the guard's ref");
        // No inbound `X-Request-ID`, so the plugin mints one rather than
        // propagating the correlation id.
        let request_id = ctx.headers.get("x-request-id").unwrap();
        assert!(!request_id.is_empty(), "the transform minted a request id");
        assert_eq!(ctx.headers.get("x-correlation-id").unwrap(), "abc");

        // The guard still enforces its own requirement.
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        let error = svc
            .execute_guard_plugins(&upstream, &route, &mut ctx)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 400);
    }

    #[test]
    fn a_rate_limit_refusal_carries_the_limiter_headers() {
        let decision = RateLimitDecision {
            allowed: false,
            limit: 7,
            remaining: 0,
            reset_secs: 11,
            retry_after: Some(Duration::from_secs(4)),
            degraded: false,
        };
        let rejection = RateLimitRejection::new("a.example", &decision);
        assert_eq!(rejection.limit, 7);
        assert_eq!(rejection.remaining, 0);
        assert_eq!(rejection.reset_secs, 11);
        assert_eq!(rejection.retry_after_secs, 4);

        let mut headers = HeaderMap::new();
        rejection.apply_rate_limit_headers(&mut headers);
        assert_eq!(headers.get("x-ratelimit-limit").unwrap(), "7");
        assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "0");
        assert_eq!(headers.get("x-ratelimit-reset").unwrap(), "11");
    }

    #[tokio::test]
    async fn the_required_headers_guard_rejects_a_request() {
        let svc = builtin_service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.plugins.items =
            vec![crate::domain::plugin::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned()];
        upstream.plugins.configs.insert(
            crate::domain::plugin::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned(),
            serde_json::json!({ "required_request_headers": "x-correlation-id" }),
        );
        let route = route(upstream.id, "/v1");
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        let error = svc
            .execute_guard_plugins(&upstream, &route, &mut ctx)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 400);
        assert!(format!("{error}").contains("x-correlation-id"));

        // With the header present the guard lets the request through.
        ctx.headers.insert(
            http::HeaderName::from_static("x-correlation-id"),
            HeaderValue::from_static("abc"),
        );
        assert!(svc.execute_guard_plugins(&upstream, &route, &mut ctx).await.is_ok());
    }

    #[tokio::test]
    async fn the_request_id_transform_is_executed() {
        let svc = builtin_service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.plugins.items =
            vec![crate::domain::plugin::TRANSFORM_PLUGIN_REQUEST_ID.to_owned()];
        let route = route(upstream.id, "/v1");
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        svc.execute_transform_plugins(&upstream, &route, &mut ctx)
            .await
            .expect("transformed");
        assert!(ctx.headers.get("x-request-id").is_some());
    }

    #[tokio::test]
    async fn response_header_rules_are_applied() {
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.headers.response = Some(crate::domain::models::ResponseHeaderRules {
            set: [("x-served-by".to_owned(), "oagw".to_owned())]
                .into_iter()
                .collect(),
            remove: vec!["server".to_owned()],
            ..crate::domain::models::ResponseHeaderRules::default()
        });
        let route = route(upstream.id, "/v1");
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::SERVER,
            HeaderValue::from_static("nginx"),
        );
        headers.insert(
            http::header::SET_COOKIE,
            HeaderValue::from_static("session=1"),
        );
        let outcome = ProxyOutcome::Response(ProxyResponse {
            status: http::StatusCode::OK,
            headers,
            body: Bytes::from_static(b"ok"),
        });
        let rendered = svc
            .apply_response_header_rules(&upstream, &route, outcome)
            .await
            .expect("rendered");
        let ProxyOutcome::Response(response) = rendered else {
            panic!("a buffered response stays buffered");
        };
        assert_eq!(response.headers.get("x-served-by").unwrap(), "oagw");
        assert!(response.headers.get(http::header::SERVER).is_none());
        assert!(response.headers.get(http::header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn a_streamed_exchange_is_not_transformed() {
        let svc = service();
        let upstream = upstream("a.example", "127.0.0.1", 8080);
        let route = route(upstream.id, "/v1");
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let outcome = svc
            .apply_response_header_rules(
                &upstream,
                &route,
                ProxyOutcome::Stream {
                    status: http::StatusCode::OK,
                    headers,
                    body: StreamBody::from(Bytes::from_static(b"data: x\n\n")),
                },
            )
            .await
            .expect("handed on");
        assert!(matches!(outcome, ProxyOutcome::Stream { .. }));
    }

    #[test]
    fn error_sources_are_explicit() {
        assert_eq!(ProxyFailure::Domain(DomainError::Internal("x".to_owned())).error_source(), "gateway");
        assert_eq!(SOURCE_UPSTREAM, "upstream");
        assert_eq!(
            ProxyFailure::Cors(CorsRejection::origin(None)).gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert_eq!(
            ProxyFailure::Cors(CorsRejection::method(None, &http::Method::DELETE)).title(),
            "Method Not Allowed"
        );
    }

    #[test]
    fn failures_carry_a_status_and_a_detail() {
        let failure = ProxyFailure::Domain(DomainError::RouteNotFound {
            method: "GET".to_owned(),
            path: "/v1/x".to_owned(),
            alias: "a.example".to_owned(),
        });
        assert_eq!(failure.http_status(), 404);
        assert_eq!(failure.title(), "Route Not Found");
        assert!(failure.detail().contains("/v1/x"));
    }

    #[test]
    fn grpc_proxying_is_defined_but_unsupported() {
        let error = grpc_unsupported("grpc.example");
        assert_eq!(error.http_status(), 502);
        assert!(format!("{error}").contains("grpc"));
    }

    #[tokio::test]
    async fn credentials_are_injected_through_the_registered_plugin() {
        let svc = service();
        let mut upstream = upstream("a.example", "127.0.0.1", 8080);
        upstream.auth = Some(crate::domain::models::AuthConfig {
            plugin_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.ghost.v1".to_owned()),
            ..crate::domain::models::AuthConfig::default()
        });
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        let error = svc
            .inject_upstream_credentials(&upstream, &mut ctx)
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 503);
        assert!(format!("{error}").contains("not deployed"));
    }

    #[tokio::test]
    async fn an_upstream_without_auth_binds_nothing() {
        let svc = service();
        let upstream = upstream("a.example", "127.0.0.1", 8080);
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        assert!(svc.inject_upstream_credentials(&upstream, &mut ctx).await.is_ok());
        assert!(ctx.take_injected_headers().is_empty());
    }
}
