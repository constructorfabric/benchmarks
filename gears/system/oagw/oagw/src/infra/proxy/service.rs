//! Data Plane implementation.
//!
//! Executes the proxy lifecycle: config resolution (Control Plane), CORS,
//! rate limit, circuit breaker, plugin chain, outbound call, response plugin
//! chain and metric / audit emission. Bodies are streamed in both directions
//! and never materialised.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use http::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::Mutex;
use toolkit_security::SecurityContext;

use crate::domain::error::{DomainError, error_metric_type};
use crate::domain::model::{CorsConfig, Endpoint, EndpointScheme, PassthroughMode};
use crate::domain::routing;
use crate::domain::services::management::{ControlPlaneService, ResolvedTarget};
use crate::domain::services::proxy::{DataPlaneService, ProxyOutcome, ProxyRequest, RequestMeta};
use crate::infra::metrics::{ErrorLabels, OagwMetrics, state_label};
use crate::infra::plugin::BuiltinPlugins;
use crate::infra::proxy::client::{OutboundClient, RequestBodyError};
use crate::infra::ratelimit::{CircuitBreaker, CircuitState, RateDecision, RateLimiterRegistry};

/// Hop-by-hop headers never forwarded upstream (RFC 9110 §7.6.1).
pub const HOP_BY_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers that describe the gateway's own decision, never forwarded.
pub const ROUTING_HEADERS: [&str; 2] = ["x-oagw-target-host", "host"];

/// `X-OAGW-Error-Source` value for relayed upstream responses.
pub const ERROR_SOURCE_UPSTREAM: HeaderValue = HeaderValue::from_static("upstream");

/// `X-OAGW-Error-Source` value for gateway-originated responses.
pub const ERROR_SOURCE_GATEWAY: HeaderValue = HeaderValue::from_static("gateway");

/// Ceiling of the upstream connection establishment phase, in seconds.
///
/// The TCP connect, the TLS handshake and the HTTP/1.1 handshake share this
/// budget. `min(proxy_timeout, 10s)`: never longer than the proxy timeout the
/// configuration asks for, never shorter than a 10 s ceiling, so a socket that
/// accepts and then never speaks always produces a `ConnectionTimeout`.
pub const MAX_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Hard request-payload limit (`DESIGN.md` §Body Validation Rules).
pub const MAX_PAYLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Data Plane configuration, resolved once at gear init.
#[derive(Debug, Clone, Copy)]
pub struct ProxyOptions {
    /// Timeout for the upstream response head and for body idleness.
    pub proxy_timeout_secs: u64,
    /// Whether cleartext HTTP upstreams are allowed.
    pub allow_http_upstream: bool,
    /// Whether the SSRF guard is enforced.
    pub ssrf_enabled: bool,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_enabled: true,
        }
    }
}

/// Plugin registries the Data Plane resolves bindings against.
#[derive(Clone)]
pub struct PluginRegistries {
    /// Auth plugin registry.
    pub auth: Arc<crate::domain::plugin::AuthPluginRegistry>,
    /// Guard plugin registry.
    pub guards: Arc<crate::domain::plugin::GuardPluginRegistry>,
    /// Transform plugin registry.
    pub transforms: Arc<crate::domain::plugin::TransformPluginRegistry>,
}

/// The concrete [`DataPlaneService`].
pub struct DataPlaneServiceImpl {
    control_plane: Arc<dyn ControlPlaneService>,
    client: OutboundClient,
    plugins: PluginRegistries,
    metrics: OagwMetrics,
    limiters: RateLimiterRegistry,
    breakers: DashMap<uuid::Uuid, Arc<CircuitBreaker>>,
    round_robin: Mutex<u64>,
    in_flight: DashMap<String, i64>,
    /// Proxy addresses whose `Forwarded` / `X-Forwarded-For` may be believed.
    ///
    /// Empty by default: without a trusted-proxy list the client-supplied
    /// forwarding headers are ignored and the `ip` rate-limit scope falls back
    /// to the `unknown` bucket, so a client cannot mint a fresh bucket per
    /// request by rotating the header.
    trusted_proxies: Vec<TrustedProxy>,
    options: ProxyOptions,
}

impl std::fmt::Debug for DataPlaneServiceImpl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DataPlaneServiceImpl")
            .field("trusted_proxies", &self.trusted_proxies.len())
            .field("options", &self.options)
            .finish()
    }
}

/// Borrowed request context threaded through the private pipeline, so the
/// relay, the upgrade path and the audit line all describe the same call.
struct RequestScope<'a> {
    ctx: &'a SecurityContext,
    request: &'a RequestMeta,
    /// Correlation identifier of this request (inbound `X-Request-ID` or fresh).
    request_id: &'a str,
    alias: &'a str,
    path_suffix: &'a str,
}

/// Result of the admission checks shared by the relay and upgrade paths.
///
/// Dropping it releases the half-open probe slot the request owned, on every
/// exit path (early rejection included).
struct Admission {
    breaker: Arc<CircuitBreaker>,
    /// Whether this request owns the single half-open probe slot.
    probe_slot: bool,
    /// `X-RateLimit-*` values to stamp on the response.
    rate: Option<RateLimitHeaders>,
}

impl Drop for Admission {
    fn drop(&mut self) {
        if self.probe_slot {
            self.breaker.end_probe();
        }
    }
}

/// `X-RateLimit-*` response-header values (ADR-0003).
#[derive(Debug, Clone, Copy)]
struct RateLimitHeaders {
    limit: u32,
    remaining: u32,
    reset_seconds: u64,
}

impl RateLimitHeaders {
    fn of(decision: &RateDecision) -> Self {
        Self {
            limit: decision.limit(),
            remaining: decision.remaining(),
            reset_seconds: decision.reset_seconds(),
        }
    }

    fn apply(&self, headers: &mut HeaderMap) {
        let values = [
            ("x-ratelimit-limit", self.limit.to_string()),
            ("x-ratelimit-remaining", self.remaining.to_string()),
            ("x-ratelimit-reset", self.reset_seconds.to_string()),
        ];
        for (name, value) in values {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                headers.insert(name, value);
            }
        }
    }
}

impl DataPlaneServiceImpl {
    /// Wires the Data Plane to its Control Plane, transport and plugin set.
    #[must_use]
    pub fn new(
        control_plane: Arc<dyn ControlPlaneService>,
        plugins: BuiltinPlugins,
        metrics: OagwMetrics,
        options: ProxyOptions,
    ) -> Self {
        let connect_budget = connect_budget(options.proxy_timeout_secs);
        Self {
            control_plane,
            client: OutboundClient::new(options.allow_http_upstream, connect_budget),
            plugins: PluginRegistries {
                auth: plugins.auth,
                guards: plugins.guards,
                transforms: plugins.transforms,
            },
            metrics,
            limiters: RateLimiterRegistry::new(),
            breakers: DashMap::new(),
            round_robin: Mutex::new(0),
            in_flight: DashMap::new(),
            trusted_proxies: Vec::new(),
            options,
        }
    }

    /// Configures the trusted proxy set for `Forwarded` / `X-Forwarded-For`.
    ///
    /// Each entry is a bare IP or a CIDR range (`10.0.0.0/8`, `fd00::/8`). The
    /// list is empty by default, which ignores client-supplied forwarding
    /// headers entirely. Unparsable entries are dropped and logged rather than
    /// rejected.
    #[must_use]
    pub fn with_trusted_proxies(mut self, proxies: Vec<String>) -> Self {
        self.trusted_proxies = proxies
            .iter()
            .filter_map(|entry| match TrustedProxy::parse(entry) {
                Some(proxy) => Some(proxy),
                None => {
                    tracing::warn!(entry = %entry, "dropping unparsable trusted proxy entry");
                    None
                }
            })
            .collect();
        self
    }

    /// The metric handle, shared with the transport layer.
    #[must_use]
    pub fn metrics(&self) -> &OagwMetrics {
        &self.metrics
    }

    fn breaker(&self, upstream_id: uuid::Uuid) -> Arc<CircuitBreaker> {
        self.breakers
            .entry(upstream_id)
            .or_insert_with(|| Arc::new(CircuitBreaker::default_breaker()))
            .value()
            .clone()
    }

    fn next_round_robin(&self) -> u64 {
        let mut guard = self.round_robin.lock();
        *guard = guard.wrapping_add(1);
        *guard
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.options.proxy_timeout_secs.max(1))
    }

    fn target_host<'a>(&self, request: &'a RequestMeta) -> Option<&'a str> {
        request
            .headers
            .get("x-oagw-target-host")
            .and_then(|value| value.to_str().ok())
    }

    /// How the endpoint was selected, for the `selection_method` label.
    fn selection_method(&self, request: &RequestMeta, endpoints: &[Endpoint]) -> &'static str {
        if self.target_host(request).is_some() {
            "explicit_header"
        } else if endpoints.len() > 1 {
            "round_robin"
        } else {
            "default"
        }
    }

    /// Publishes the in-flight gauge for the duration of one upstream call.
    fn enter_relay(&self, host: &str) {
        let mut current = self.in_flight.entry(host.to_owned()).or_insert(0);
        *current += 1;
        let value = *current;
        drop(current);
        self.metrics.in_flight(host, value);
    }

    /// Releases the in-flight gauge slot taken by [`enter_relay`].
    fn exit_relay(&self, host: &str) {
        let mut current = self.in_flight.entry(host.to_owned()).or_insert(0);
        *current = (*current - 1).max(0);
        let value = *current;
        drop(current);
        self.metrics.in_flight(host, value);
    }

    /// Validates the endpoint against the SSRF policy.
    ///
    /// # Errors
    ///
    /// 400 [`DomainError::InvalidTargetHost`] for any address that is not a
    /// globally routable public unicast address: loopback, private,
    /// link-local, cloud metadata, shared address space, benchmarking,
    /// multicast, reserved and the IPv4-mapped / IPv4-compatible IPv6 forms.
    fn validate_endpoint(&self, endpoint: &Endpoint) -> Result<(), DomainError> {
        if !self.options.ssrf_enabled {
            return Ok(());
        }
        if let Ok(address) = endpoint.host.parse::<IpAddr>()
            && is_blocked_address(address)
        {
            return Err(DomainError::InvalidTargetHost(format!(
                "endpoint '{}' is not a routable public address (SSRF policy)",
                endpoint.host
            )));
        }
        Ok(())
    }

    /// Enforces the inbound body rules of `DESIGN.md` §Body Validation Rules.
    ///
    /// A request without a declared `Content-Length` cannot be bounded here; it
    /// is counted while it streams ([`limited_body`]).
    ///
    /// # Errors
    ///
    /// 400 [`DomainError::Validation`] for an unsupported `Transfer-Encoding`
    /// or a malformed `Content-Length` and 413
    /// [`DomainError::PayloadTooLarge`] above the 100 MB hard limit.
    fn enforce_payload_rules(request: &RequestMeta) -> Result<(), DomainError> {
        if let Some(encoding) = request
            .headers
            .get(http::header::TRANSFER_ENCODING)
            .and_then(|value| value.to_str().ok())
            && !encoding
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        {
            return Err(DomainError::Validation(
                "only chunked transfer encoding is supported".to_owned(),
            ));
        }
        let Some(value) = request
            .headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
        else {
            return Ok(());
        };
        let length = value.trim().parse::<u64>().map_err(|_| {
            DomainError::Validation("content-length is not a valid integer".to_owned())
        })?;
        if length > MAX_PAYLOAD_BYTES {
            return Err(DomainError::PayloadTooLarge(format!(
                "payload of {length} bytes exceeds the {} byte limit",
                MAX_PAYLOAD_BYTES
            )));
        }
        Ok(())
    }

    fn enforce_cors(
        &self,
        target: &ResolvedTarget,
        request: &RequestMeta,
    ) -> Result<(), DomainError> {
        crate::domain::cors::validate_actual_request(
            target.cors.as_ref(),
            request.headers.get(http::header::ORIGIN),
            request.method.as_str(),
        )
    }

    /// Enforces the effective rate limit.
    ///
    /// # Errors
    ///
    /// 429 [`DomainError::RateLimitExceeded`] when the bucket is exhausted.
    fn check_rate_limit(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
    ) -> Result<Option<RateLimitHeaders>, DomainError> {
        let Some(limit) = target.rate_limit.as_ref() else {
            return Ok(None);
        };
        let client = forwarded_client_ip(&scope.request.headers, &self.trusted_proxies)
            .map(|ip| ip.to_string());
        let key = RateLimiterRegistry::scope_key(
            limit.scope,
            target.upstream.id,
            Some(target.route.id),
            scope.ctx.subject_tenant_id(),
            scope.ctx.subject_id(),
            client.as_deref(),
        );
        let decision: RateDecision = self.limiters.check(&key, limit);
        if decision.is_allowed() {
            return Ok(limit
                .response_headers
                .then(|| RateLimitHeaders::of(&decision)));
        }
        self.metrics.rate_limit_exceeded(
            &target.upstream.alias,
            &target.route.match_key(),
            self.limiters.usage_ratio(&key, limit.capacity()),
        );
        Err(DomainError::RateLimitExceeded {
            detail: format!("rate limit of {} exceeded", decision.limit()),
            retry_after_seconds: decision.retry_after(),
        })
    }

    /// Guard sequence shared by the relay and the upgrade paths.
    ///
    /// Payload rules, CORS, rate limit and the circuit breaker all run here so
    /// the two paths cannot diverge. In [`CircuitState::HalfOpen`] exactly one
    /// request owns the probe slot; every other request is rejected with 503
    /// until the probe succeeds or fails.
    ///
    /// # Errors
    ///
    /// The first rejection: 400/413 payload, 403 CORS, 429 rate limit or 503
    /// circuit breaker.
    fn admit(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
    ) -> Result<Admission, DomainError> {
        Self::enforce_payload_rules(scope.request)?;
        self.enforce_cors(target, scope.request)?;
        let rate = self.check_rate_limit(target, scope)?;
        let breaker = self.breaker(target.upstream.id);
        let state = breaker.state();
        self.metrics.circuit_state(&target.upstream.alias, state);
        if state == CircuitState::Open {
            return Err(DomainError::CircuitBreakerOpen {
                detail: format!("circuit breaker is open for '{}'", target.upstream.alias),
                retry_after_seconds: breaker.retry_after_seconds(),
            });
        }
        let probe_slot = state == CircuitState::HalfOpen && breaker.begin_probe();
        if state == CircuitState::HalfOpen && !probe_slot {
            return Err(DomainError::CircuitBreakerOpen {
                detail: format!(
                    "circuit breaker is probing for '{}'; one request at a time is admitted",
                    target.upstream.alias
                ),
                retry_after_seconds: 1,
            });
        }
        Ok(Admission {
            breaker,
            probe_slot,
            rate,
        })
    }

    fn outbound_headers(
        &self,
        target: &ResolvedTarget,
        request: &RequestMeta,
        endpoint_host: &str,
        is_upgrade: bool,
    ) -> Result<HeaderMap, DomainError> {
        let rules = target
            .headers
            .as_ref()
            .and_then(|config| config.request.clone());
        let passthrough = rules
            .as_ref()
            .map_or(PassthroughMode::None, |r| r.passthrough);
        let allowlist = rules
            .as_ref()
            .map_or(Vec::new(), |r| r.passthrough_allowlist.clone());
        let strip = hop_by_hop_strip_set(request.headers.get(http::header::CONNECTION));

        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            let lower = name.as_str().to_ascii_lowercase();
            if strip.iter().any(|stripped| stripped.as_str() == lower)
                || ROUTING_HEADERS.contains(&lower.as_str())
            {
                continue;
            }
            if !is_upgrade && lower == "authorization" {
                // Client credentials are never forwarded upstream implicitly.
                continue;
            }
            let forward = match passthrough {
                PassthroughMode::None => false,
                PassthroughMode::Allowlist => allowlist
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&lower)),
                PassthroughMode::All => true,
            };
            if forward {
                // Repeated headers (e.g. several `Set-Cookie` values) survive.
                headers.append(name.clone(), value.clone());
            }
        }
        if let Some(rules) = rules.as_ref() {
            apply_request_rules(rules, &mut headers);
        }
        if let Ok(value) = HeaderValue::from_str(endpoint_host) {
            headers.insert(http::header::HOST, value);
        }
        Ok(headers)
    }

    fn apply_response_rules(&self, target: &ResolvedTarget, parts: &mut http::response::Parts) {
        let Some(rules) = target
            .headers
            .as_ref()
            .and_then(|config| config.response.clone())
        else {
            return;
        };
        for name in &rules.remove {
            if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
                parts.headers.remove(&parsed);
            }
        }
        for (name, value) in &rules.set {
            if let (Ok(parsed), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                parts.headers.insert(parsed, value);
            }
        }
        for (name, value) in &rules.add {
            if let (Ok(parsed), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                parts.headers.append(parsed, value);
            }
        }
    }

    fn plugin_context(
        &self,
        scope: &RequestScope<'_>,
        target: &ResolvedTarget,
        endpoint: &Endpoint,
    ) -> crate::domain::plugin::PluginContext {
        crate::domain::plugin::PluginContext {
            security_context: scope.ctx.clone(),
            upstream_id: target.upstream.id,
            host: target.upstream.alias.clone(),
            route_id: Some(target.route.id),
            endpoint_host: endpoint_authority(endpoint),
            request_id: scope.request_id.to_owned(),
        }
    }

    async fn run_auth(
        &self,
        target: &ResolvedTarget,
        plugin_ctx: &crate::domain::plugin::PluginContext,
        parts: &mut http::request::Parts,
    ) -> Result<(), DomainError> {
        let Some(binding) = target.plugin_chain.auth.as_ref() else {
            return Ok(());
        };
        let plugin = self.plugins.auth.resolve(&binding.plugin_ref)?;
        plugin
            .authenticate(
                plugin_ctx,
                &plugin_ctx.security_context,
                &binding.config,
                parts,
            )
            .await
    }

    async fn run_guards(
        &self,
        target: &ResolvedTarget,
        plugin_ctx: &crate::domain::plugin::PluginContext,
        parts: &http::request::Parts,
    ) -> Result<(), DomainError> {
        for binding in &target.plugin_chain.guards {
            let plugin = self.plugins.guards.resolve(&binding.plugin_ref)?;
            plugin
                .guard_request(plugin_ctx, &binding.config, parts)
                .await?;
        }
        Ok(())
    }

    async fn run_request_transforms(
        &self,
        target: &ResolvedTarget,
        plugin_ctx: &crate::domain::plugin::PluginContext,
        parts: &mut http::request::Parts,
    ) -> Result<(), DomainError> {
        for binding in &target.plugin_chain.transforms {
            let plugin = self.plugins.transforms.resolve(&binding.plugin_ref)?;
            plugin
                .transform_request(plugin_ctx, &binding.config, parts)
                .await?;
        }
        Ok(())
    }

    async fn run_response_transforms(
        &self,
        target: &ResolvedTarget,
        plugin_ctx: &crate::domain::plugin::PluginContext,
        parts: &mut http::response::Parts,
    ) -> Result<(), DomainError> {
        for binding in target.plugin_chain.transforms.iter().rev() {
            let plugin = self.plugins.transforms.resolve(&binding.plugin_ref)?;
            plugin
                .transform_response(plugin_ctx, &binding.config, parts)
                .await?;
        }
        Ok(())
    }

    async fn run_response_guards(
        &self,
        target: &ResolvedTarget,
        plugin_ctx: &crate::domain::plugin::PluginContext,
        parts: &http::response::Parts,
    ) -> Result<(), DomainError> {
        for binding in target.plugin_chain.guards.iter().rev() {
            let plugin = self.plugins.guards.resolve(&binding.plugin_ref)?;
            plugin
                .guard_response(plugin_ctx, &binding.config, parts)
                .await?;
        }
        Ok(())
    }

    /// The single rejection sink: one metric set and one audit line.
    ///
    /// Called for every error that leaves [`DataPlaneService::proxy`], whether
    /// the request never got a target (unresolved alias) or was rejected after
    /// resolution, so nothing is counted twice and no request goes uncounted.
    fn record_rejection(
        &self,
        target: Option<&ResolvedTarget>,
        scope: &RequestScope<'_>,
        started: Instant,
        error: &DomainError,
    ) {
        let host = target.map_or_else(
            || scope.alias.to_owned(),
            |resolved| resolved.upstream.alias.clone(),
        );
        let route = target.map_or_else(
            || "unresolved".to_owned(),
            |resolved| resolved.route.match_key(),
        );
        let elapsed = started.elapsed();
        self.metrics.record_request(
            &host,
            &route,
            scope.request.method.as_str(),
            0,
            elapsed.as_secs_f64(),
        );
        self.metrics.record_error(ErrorLabels {
            host: &host,
            route: &route,
            error_code: error_metric_type(error),
        });
        audit(target, scope, None, error.status(), elapsed, Some(error));
    }

    /// Records the request metric and the audit line of a relayed request.
    fn finish(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
        endpoint: Option<&Endpoint>,
        status: u16,
        elapsed: Duration,
    ) {
        self.metrics.record_request(
            &target.upstream.alias,
            &target.route.match_key(),
            scope.request.method.as_str(),
            status,
            elapsed.as_secs_f64(),
        );
        audit(Some(target), scope, endpoint, status, elapsed, None);
    }

    /// Answers a CORS preflight locally, without touching the upstream.
    ///
    /// Returns [`None`] when the request is not a preflight or the effective
    /// CORS configuration is missing or disabled, in which case the caller
    /// falls through to the normal relay path and the non-CORS behaviour is
    /// unchanged. Otherwise the request is answered here: 204 with the CORS
    /// headers, or 403 for an origin outside `allowed_origins`.
    #[allow(clippy::too_many_lines)]
    fn preflight(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
        started: Instant,
    ) -> Option<Result<ProxyOutcome, DomainError>> {
        if !crate::domain::cors::is_preflight(scope.request.method.as_str(), &scope.request.headers)
        {
            return None;
        }
        let cors = target.cors.as_ref().filter(|config| config.enabled)?;
        let origin = scope
            .request
            .headers
            .get(http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())?;
        let host = target.upstream.alias.as_str();
        let route = target.route.match_key();
        if !crate::domain::cors::origin_allowed(cors, origin) {
            // The caller's rejection sink owns the metric and the audit line.
            return Some(Err(DomainError::CorsOriginNotAllowed(format!(
                "origin '{origin}' is not in the upstream allowed_origins"
            ))));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in preflight_headers(cors, origin, &scope.request.headers) {
            headers.insert(name, value);
        }
        let outcome = ProxyOutcome {
            status: http::StatusCode::NO_CONTENT,
            headers,
            body: axum::body::Body::empty(),
        };
        self.metrics.record_request(
            host,
            &route,
            "OPTIONS",
            204,
            started.elapsed().as_secs_f64(),
        );
        audit(Some(target), scope, None, 204, started.elapsed(), None);
        Some(Ok(outcome))
    }

    /// Stamps the cross-origin response headers of ADR-0004.
    ///
    /// Runs after the response plugin chain and the header rules, so the
    /// gateway's own CORS decision wins over anything the upstream or a
    /// transform plugin put in the message.
    fn apply_cors_response_headers(
        &self,
        target: &ResolvedTarget,
        request: &RequestMeta,
        headers: &mut HeaderMap,
    ) {
        let Some(cors) = target.cors.as_ref().filter(|config| config.enabled) else {
            return;
        };
        let Some(origin) = request
            .headers
            .get(http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
        else {
            return;
        };
        if !crate::domain::cors::origin_allowed(cors, origin) {
            return;
        }
        for (name, value) in cors_response_headers(cors, origin) {
            headers.insert(name, value);
        }
    }
}

#[async_trait]
impl DataPlaneService for DataPlaneServiceImpl {
    /// Entry point of the Data Plane.
    ///
    /// Rejection bookkeeping lives in exactly one place: whatever the cause
    /// (config resolution, CORS preflight, admission or the upstream call), a
    /// returned error is counted once and audited once.
    #[allow(clippy::too_many_lines)]
    async fn proxy(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        path_suffix: &str,
        request: ProxyRequest,
    ) -> Result<ProxyOutcome, DomainError> {
        let started = Instant::now();
        // The body is moved out once; the metadata stays borrowable for the
        // whole lifecycle so metric labels can be emitted after the relay.
        let (meta, body) = request.into_parts();
        let request_id =
            crate::domain::plugin::PluginContext::request_id_from_headers(&meta.headers);
        let scope = RequestScope {
            ctx,
            request: &meta,
            request_id: &request_id,
            alias,
            path_suffix,
        };
        let resolved = self
            .control_plane
            .resolve_proxy_target(ctx, alias, meta.method.as_str(), &meta.path, path_suffix)
            .await;
        let outcome = match &resolved {
            Ok(target) => self.handle(target, &scope, body, started).await,
            Err(error) => Err(error.clone()),
        };
        if let Err(error) = &outcome {
            self.record_rejection(resolved.ok().as_ref(), &scope, started, error);
        }
        outcome
    }
}

impl DataPlaneServiceImpl {
    /// Dispatches one resolved request: preflight shortcut, then the relay.
    async fn handle(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
        body: axum::body::Body,
        started: Instant,
    ) -> Result<ProxyOutcome, DomainError> {
        // A preflight is answered here: the upstream is never contacted for one.
        if let Some(answer) = self.preflight(target, scope, started) {
            return answer;
        }
        self.dispatch(target, scope, body, started).await
    }

    async fn dispatch(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
        body: axum::body::Body,
        started: Instant,
    ) -> Result<ProxyOutcome, DomainError> {
        let admission = self.admit(target, scope)?;
        let selected = self.select_endpoint(target, scope.request);
        let result = match &selected {
            Err(error) => Err(error.clone()),
            Ok(endpoint) => {
                let authority = endpoint_authority(endpoint);
                let outcome = self
                    .execute(target, scope, endpoint, body, &admission)
                    .await;
                self.metrics.upstream_available(
                    &target.upstream.alias,
                    &authority,
                    !Self::is_transport_failure(&outcome),
                );
                outcome
            }
        };
        let transition = if Self::is_transport_failure(&result) {
            admission.breaker.record_failure()
        } else if result.is_ok() {
            admission.breaker.record_success()
        } else {
            None
        };
        if let Some((from, to)) = transition {
            self.metrics.circuit_transition(
                &target.upstream.alias,
                state_label(from),
                state_label(to),
            );
            self.metrics.circuit_state(&target.upstream.alias, to);
        }
        if let Ok(outcome) = &result {
            self.finish(
                target,
                scope,
                selected.as_ref().ok(),
                outcome.status.as_u16(),
                started.elapsed(),
            );
        }
        result
    }

    /// Selects and SSRF-validates the endpoint for this request.
    fn select_endpoint(
        &self,
        target: &ResolvedTarget,
        request: &RequestMeta,
    ) -> Result<Endpoint, DomainError> {
        let endpoint = routing::select_endpoint(
            &target.upstream.server.endpoints,
            target.alias_is_common_suffix,
            self.target_host(request),
            self.next_round_robin(),
        )?;
        self.validate_endpoint(&endpoint)?;
        let authority = endpoint_authority(&endpoint);
        let method = self.selection_method(request, &target.upstream.server.endpoints);
        self.metrics
            .endpoint_selected(&target.upstream.id.to_string(), &authority, method);
        if self.target_host(request).is_some() {
            self.metrics
                .target_host_used(&target.upstream.id.to_string(), &authority);
        }
        Ok(endpoint)
    }

    /// Whether `result` describes a transport-level upstream failure, i.e. one
    /// that must count towards the circuit breaker.
    fn is_transport_failure(result: &Result<ProxyOutcome, DomainError>) -> bool {
        result.as_ref().err().is_some_and(Self::is_transport_error)
    }

    /// Whether `error` is a transport-level upstream failure.
    fn is_transport_error(error: &DomainError) -> bool {
        matches!(
            error,
            DomainError::DownstreamError(_)
                | DomainError::RequestTimeout(_)
                | DomainError::LinkUnavailable { .. }
                | DomainError::ConnectionTimeout(_)
                | DomainError::StreamAborted(_)
                | DomainError::ProtocolError(_)
        )
    }

    /// Builds and sends the outbound request, then relays the response.
    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
        endpoint: &Endpoint,
        body: axum::body::Body,
        admission: &Admission,
    ) -> Result<ProxyOutcome, DomainError> {
        let http_match = target
            .route
            .match_config
            .http
            .as_ref()
            .ok_or_else(|| DomainError::RouteNotFound("route has no HTTP match".to_owned()))?;
        routing::validate_query_params(http_match, &scope.request.query)?;
        let upstream_path = routing::apply_path_suffix(http_match, scope.path_suffix)?;
        let scheme = scheme_of(endpoint);

        let mut headers = self.outbound_headers(
            target,
            scope.request,
            &endpoint_authority(endpoint),
            scope.request.is_upgrade,
        )?;
        let mut outbound = http::Request::builder()
            .method(scope.request.method.clone())
            .version(http::Version::HTTP_11)
            .uri(build_uri(
                &scheme,
                &endpoint.host,
                endpoint.port,
                &upstream_path,
                &scope.request.query,
            )?)
            .body(limited_body(body, MAX_PAYLOAD_BYTES))
            .map_err(|_| {
                DomainError::ProtocolError("cannot build the outbound request".to_owned())
            })?;
        *outbound.headers_mut() = std::mem::take(&mut headers);

        let (mut parts, body) = outbound.into_parts();
        let plugin_ctx = self.plugin_context(scope, target, endpoint);
        self.run_auth(target, &plugin_ctx, &mut parts).await?;
        self.run_guards(target, &plugin_ctx, &parts).await?;
        self.run_request_transforms(target, &plugin_ctx, &mut parts)
            .await?;
        parts.headers.remove(http::header::CONTENT_LENGTH);
        let outbound = http::Request::from_parts(parts, body);

        let authority = endpoint_authority(endpoint);
        self.enter_relay(&authority);
        let outcome = match self.client.send(outbound, self.timeout()).await {
            Ok(response) => {
                self.relay(target, scope, &plugin_ctx, response, admission)
                    .await
            }
            Err(error) => Err(error),
        };
        self.exit_relay(&authority);
        outcome
    }

    /// Turns an upstream response into a client response.
    ///
    /// The response is never buffered: the body is streamed through an
    /// idleness guard. Hop-by-hop headers (including the tokens the upstream
    /// named in its own `Connection` header) are stripped, CORS and
    /// `X-RateLimit-*` headers are stamped, and the response plugin chain runs
    /// in reverse order.
    #[allow(clippy::too_many_lines)]
    async fn relay(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
        plugin_ctx: &crate::domain::plugin::PluginContext,
        response: http::Response<hyper::body::Incoming>,
        admission: &Admission,
    ) -> Result<ProxyOutcome, DomainError> {
        let (mut parts, incoming) = response.into_parts();
        parts.headers.insert(
            http::header::HeaderName::from_static("x-oagw-error-source"),
            ERROR_SOURCE_UPSTREAM.clone(),
        );
        self.run_response_transforms(target, plugin_ctx, &mut parts)
            .await?;
        self.run_response_guards(target, plugin_ctx, &parts).await?;
        self.apply_response_rules(target, &mut parts);
        for name in hop_by_hop_strip_set(parts.headers.get(http::header::CONNECTION)) {
            parts.headers.remove(&name);
        }
        self.apply_cors_response_headers(target, scope.request, &mut parts.headers);
        if let Some(rate) = admission.rate {
            rate.apply(&mut parts.headers);
        }
        let stream = IdleTimeoutStream::new(incoming, self.timeout());
        Ok(ProxyOutcome {
            status: parts.status,
            headers: parts.headers,
            body: axum::body::Body::from_stream(stream),
        })
    }
}

/// The upstream half of a `101 Switching Protocols` exchange.
pub type UpstreamUpgrade = hyper::upgrade::OnUpgrade;

/// Request headers that negotiate a protocol switch and must reach the
/// upstream verbatim, however the hop-by-hop filter sees them.
const UPGRADE_REQUEST_HEADERS: [&str; 5] = [
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
];

impl DataPlaneServiceImpl {
    /// Relays a WebSocket upgrade.
    ///
    /// The path shares its admission sequence with the HTTP relay (payload
    /// rules, CORS, rate limit, circuit breaker) and keeps the negotiation
    /// headers — the ones the hop-by-hop filter would otherwise remove — so the
    /// upstream sees exactly the switch the client asked for. The response is
    /// the upstream `101` verbatim, and the caller receives the upstream side
    /// of the switched protocol.
    ///
    /// # Errors
    ///
    /// Any routing, plugin or transport failure; also 502 when the upstream
    /// does not answer the upgrade with `101`.
    #[allow(clippy::too_many_lines)]
    pub async fn upgrade(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        path_suffix: &str,
        request: &RequestMeta,
    ) -> Result<(http::Response<axum::body::Body>, UpstreamUpgrade), DomainError> {
        let started = Instant::now();
        let request_id =
            crate::domain::plugin::PluginContext::request_id_from_headers(&request.headers);
        let scope = RequestScope {
            ctx,
            request,
            request_id: &request_id,
            alias,
            path_suffix,
        };
        let resolved = self
            .control_plane
            .resolve_proxy_target(
                ctx,
                alias,
                scope.request.method.as_str(),
                &scope.request.path,
                path_suffix,
            )
            .await;
        let outcome = match &resolved {
            Ok(target) => self.upgrade_target(target, &scope).await,
            Err(error) => Err(error.clone()),
        };
        // Same rejection bookkeeping as the HTTP relay: one metric, one line.
        if let Err(error) = &outcome {
            self.record_rejection(resolved.ok().as_ref(), &scope, started, error);
        }
        outcome
    }

    /// The upgrade path once the target is resolved.
    #[allow(clippy::too_many_lines)]
    async fn upgrade_target(
        &self,
        target: &ResolvedTarget,
        scope: &RequestScope<'_>,
    ) -> Result<(http::Response<axum::body::Body>, UpstreamUpgrade), DomainError> {
        let path_suffix = scope.path_suffix;
        let started = Instant::now();
        let admission = self.admit(target, scope)?;
        let endpoint = self.select_endpoint(target, scope.request)?;

        let http_match = target
            .route
            .match_config
            .http
            .as_ref()
            .ok_or_else(|| DomainError::RouteNotFound("route has no HTTP match".to_owned()))?;
        let upstream_path = routing::apply_path_suffix(http_match, path_suffix)?;
        let scheme = scheme_of(&endpoint);

        let mut headers =
            self.outbound_headers(target, scope.request, &endpoint_authority(&endpoint), true)?;
        let mut outbound = http::Request::builder()
            .method(scope.request.method.clone())
            .uri(build_uri(
                &scheme,
                &endpoint.host,
                endpoint.port,
                &upstream_path,
                &scope.request.query,
            )?)
            .body(axum::body::Body::empty())
            .map_err(|_| {
                DomainError::ProtocolError("cannot build the upgrade request".to_owned())
            })?;
        *outbound.headers_mut() = std::mem::take(&mut headers);

        let (mut parts, body) = outbound.into_parts();
        let plugin_ctx = self.plugin_context(scope, target, &endpoint);
        self.run_auth(target, &plugin_ctx, &mut parts).await?;
        self.run_guards(target, &plugin_ctx, &parts).await?;
        self.run_request_transforms(target, &plugin_ctx, &mut parts)
            .await?;
        // The switch itself is hop-by-hop by design: restore the negotiation
        // headers the generic filter removed, in their original multiplicity.
        for name in UPGRADE_REQUEST_HEADERS {
            if let Some(value) = scope.request.headers.get(name) {
                parts.headers.insert(name, value.clone());
            }
        }
        parts.headers.remove(http::header::CONTENT_LENGTH);
        let outbound = http::Request::from_parts(parts, body);
        let authority = endpoint_authority(&endpoint);
        self.enter_relay(&authority);
        let upstream = self.client.send(outbound, self.timeout()).await;
        self.exit_relay(&authority);
        // The breaker and the availability gauge follow the same rules as the
        // HTTP relay, so a broken upstream is not reported differently because
        // the client asked for a switch.
        let transition = match &upstream {
            Err(error) if Self::is_transport_error(error) => admission.breaker.record_failure(),
            Ok(_) => admission.breaker.record_success(),
            Err(_) => None,
        };
        if let Some((from, to)) = transition {
            self.metrics.circuit_transition(
                &target.upstream.alias,
                state_label(from),
                state_label(to),
            );
            self.metrics.circuit_state(&target.upstream.alias, to);
        }
        self.metrics
            .upstream_available(&target.upstream.alias, &authority, upstream.is_ok());
        let mut upstream = match upstream {
            Ok(response) => response,
            Err(error) => return Err(error),
        };
        if upstream.status() != http::StatusCode::SWITCHING_PROTOCOLS {
            return Err(DomainError::ProtocolError(format!(
                "the upstream answered the upgrade with status {}",
                upstream.status()
            )));
        }
        let upgraded = hyper::upgrade::on(&mut upstream);
        let (mut parts, _) = upstream.into_parts();
        parts.headers.insert(
            http::header::HeaderName::from_static("x-oagw-error-source"),
            ERROR_SOURCE_UPSTREAM.clone(),
        );
        if let Some(rate) = admission.rate {
            rate.apply(&mut parts.headers);
        }
        self.finish(
            target,
            scope,
            Some(&endpoint),
            parts.status.as_u16(),
            started.elapsed(),
        );
        Ok((
            http::Response::from_parts(parts, axum::body::Body::empty()),
            upgraded,
        ))
    }
}

/// Applies upstream request-header rules to an already-filtered header map.
fn apply_request_rules(rules: &crate::domain::model::RequestHeaderRules, headers: &mut HeaderMap) {
    for name in &rules.remove {
        if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&parsed);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(parsed), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(parsed, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(parsed), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(parsed, value);
        }
    }
}

/// `host:port` authority of an endpoint, IPv6-literal hosts bracketed.
///
/// An IPv6 host must appear as `[2001:db8::1]` in an authority; without the
/// brackets the URI (and the `Host` header) is unparseable.
#[must_use]
pub fn endpoint_authority(endpoint: &Endpoint) -> String {
    format!("{}:{}", bracketed(&endpoint.host), endpoint.port)
}

/// Brackets an IPv6 literal, leaving every other host form untouched.
#[must_use]
pub fn bracketed(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// Outbound scheme for an endpoint.
#[must_use]
pub fn scheme_of(endpoint: &Endpoint) -> String {
    match endpoint.scheme {
        EndpointScheme::Http => "http".to_owned(),
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Grpc => "https".to_owned(),
        EndpointScheme::Wt => "https".to_owned(),
    }
}

/// Builds the outbound absolute URI.
///
/// # Errors
///
/// 400 [`DomainError::Validation`] when the resulting URI is not valid.
pub fn build_uri(
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    query: &str,
) -> Result<http::Uri, DomainError> {
    let path_and_query = if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    };
    http::Uri::builder()
        .scheme(scheme)
        .authority(format!("{}:{port}", bracketed(host)))
        .path_and_query(path_and_query)
        .build()
        .map_err(|error| DomainError::Validation(format!("invalid outbound URI: {error}")))
}

/// Connection-establishment budget for one upstream call.
///
/// The TCP connect, the TLS handshake and the HTTP/1.1 handshake share it:
/// never longer than the configured proxy timeout, never longer than
/// [`MAX_CONNECT_TIMEOUT_SECS`], and never under a second.
#[must_use]
pub fn connect_budget(proxy_timeout_secs: u64) -> Duration {
    Duration::from_secs(proxy_timeout_secs.clamp(1, MAX_CONNECT_TIMEOUT_SECS))
}

/// A trusted reverse proxy: a bare address or a CIDR range.
#[derive(Debug, Clone, Copy)]
enum TrustedProxy {
    Address(IpAddr),
    Range(IpAddr, u8),
}

impl TrustedProxy {
    /// Parses `a.b.c.d`, `2001:db8::1` or `10.0.0.0/8`.
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if let Some((network, prefix)) = entry.split_once('/') {
            let address: IpAddr = network.trim().parse().ok()?;
            let width = match address {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            let prefix = prefix.trim().parse::<u8>().ok()?;
            (u32::from(prefix) <= width).then_some(Self::Range(address, prefix))
        } else {
            entry.parse::<IpAddr>().ok().map(Self::Address)
        }
    }

    /// Whether `candidate` sits inside this proxy's address space.
    fn covers(&self, candidate: IpAddr) -> bool {
        match *self {
            Self::Address(address) => address == candidate,
            Self::Range(network, prefix) => match (network, candidate) {
                (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                    u32::from(network) & v4_mask(prefix) == u32::from(candidate) & v4_mask(prefix)
                }
                (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                    u128::from(network) & v6_mask(prefix) == u128::from(candidate) & v6_mask(prefix)
                }
                _ => false,
            },
        }
    }
}

/// Left-justified IPv4 network mask for a prefix length.
fn v4_mask(prefix: u8) -> u32 {
    match u32::from(prefix) {
        0 => 0,
        32 => u32::MAX,
        prefix => u32::MAX << (32 - prefix),
    }
}

/// Left-justified IPv6 network mask for a prefix length.
fn v6_mask(prefix: u8) -> u128 {
    match u32::from(prefix) {
        0 => 0,
        128 => u128::MAX,
        prefix => u128::MAX << (128 - prefix),
    }
}

/// The client address to rate-limit on, if the forwarding headers may be
/// believed.
///
/// The chain is only read when at least one trusted proxy is configured; a
/// client-supplied `Forwarded` / `X-Forwarded-For` is otherwise ignored, so a
/// caller cannot mint a fresh `ip` bucket per request by rotating the header.
/// The first list entry is used and must be a bare IP literal: `Forwarded`
/// parameters (`for=…;by=…`), junk and IPv6 without brackets are refused, and
/// an address that is itself a trusted proxy is refused too (the chain is
/// longer than the gear is configured to trust).
///
/// Deriving the address of the *peer socket* is not possible here — the Data
/// Plane sees no connection info — so a direct, unproxied request has no client
/// address and shares the `unknown` bucket.
fn forwarded_client_ip(headers: &HeaderMap, trusted: &[TrustedProxy]) -> Option<IpAddr> {
    if trusted.is_empty() {
        return None;
    }
    let value = headers
        .get(http::header::FORWARDED)
        .or_else(|| headers.get("x-forwarded-for"))
        .and_then(|value| value.to_str().ok())?;
    let first = value.split(',').next()?.trim();
    if first.is_empty() || first.contains('=') || first.contains(';') {
        return None;
    }
    let address = first
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(first)
        .parse::<IpAddr>()
        .ok()?;
    trusted
        .iter()
        .all(|proxy| !proxy.covers(address))
        .then_some(address)
}

/// Whether `address` must be refused by the SSRF policy.
///
/// Only globally routable public unicast addresses are allowed: loopback,
/// private, link-local (cloud metadata), shared address space, benchmarking,
/// documentation, multicast, broadcast, reserved, the unspecified and
/// IPv4-mapped / IPv4-compatible IPv6 forms are all blocked.
#[must_use]
pub fn is_blocked_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_blocked_ipv4(address),
        IpAddr::V6(address) => is_blocked_ipv6(address),
    }
}

/// Whether an IPv4 address is refused by the SSRF policy.
#[must_use]
pub fn is_blocked_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_multicast()
        || address.is_unspecified()
        || address.is_documentation()
        // 0.0.0.0/8, "this network": `0.1.2.3` never names a real upstream.
        || octets[0] == 0
        // 100.64.0.0/10, carrier-grade NAT (Tailscale and friends).
        || (octets[0] == 100 && octets[1] & 0xc0 == 64)
        // 192.0.0.0/24, IETF protocol assignments.
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // 198.18.0.0/15, benchmarking.
        || (octets[0] == 198 && octets[1] & 0xfe == 18)
        // 240.0.0.0/4, reserved.
        || octets[0] >= 240
}

/// Whether an IPv6 address is refused by the SSRF policy.
#[must_use]
pub fn is_blocked_ipv6(address: Ipv6Addr) -> bool {
    if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
        return true;
    }
    // `::ffff:a.b.c.d` and the deprecated `::a.b.c.d` embed an IPv4 address:
    // the same rules apply to it, so `::ffff:169.254.169.254` is refused.
    if let Some(embedded) = address.to_ipv4() {
        return is_blocked_ipv4(embedded);
    }
    let segments = address.segments();
    // fc00::/7, unique local addresses (for instance `fc00::1`).
    (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10, link-local (for instance `fe80::1`).
        || (segments[0] & 0xffc0) == 0xfe80
}

/// The hop-by-hop set to strip from an outbound message: the static list plus
/// every header the message's own `Connection` header named.
#[must_use]
pub fn hop_by_hop_strip_set(connection: Option<&HeaderValue>) -> Vec<HeaderName> {
    let mut names: Vec<HeaderName> = HOP_BY_HOP_HEADERS
        .iter()
        .filter_map(|name| HeaderName::from_bytes(name.as_bytes()).ok())
        .collect();
    for token in connection_tokens(connection) {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes())
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// Whitespace-separated tokens of a `Connection` header value, lowercased.
#[must_use]
pub fn connection_tokens(connection: Option<&HeaderValue>) -> Vec<String> {
    connection
        .and_then(|value| value.to_str().ok())
        .map_or_else(Vec::new, |value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        })
}

/// Headers of a CORS preflight response (ADR-0004).
///
/// The origin is echoed (never `*`, so a credentialed request keeps working),
/// the allowed methods are listed, the requested headers are echoed back and
/// the response varies on the three request headers that select it, so a
/// intermediary cache cannot serve one preflight for another.
#[must_use]
pub fn preflight_headers(
    cors: &CorsConfig,
    origin: &str,
    request: &HeaderMap,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = vec![
        header_pair("access-control-allow-origin", origin),
        header_pair(
            "access-control-allow-methods",
            &cors.allowed_methods.join(", "),
        ),
        header_pair(
            "access-control-max-age",
            &crate::domain::cors::PREFLIGHT_MAX_AGE_SECS.to_string(),
        ),
        header_pair("vary", crate::domain::cors::PREFLIGHT_VARY),
    ];
    if cors.allow_credentials {
        headers.push(header_pair("access-control-allow-credentials", "true"));
    }
    if let Some(requested) = request
        .get(http::header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|value| value.to_str().ok())
    {
        headers.push(header_pair("access-control-allow-headers", requested));
    }
    headers.into_iter().flatten().collect()
}

/// Headers stamped on an actual cross-origin response (ADR-0004).
#[must_use]
pub fn cors_response_headers(cors: &CorsConfig, origin: &str) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = vec![
        header_pair("access-control-allow-origin", origin),
        header_pair("vary", "Origin"),
    ];
    if !cors.expose_headers.is_empty() {
        headers.push(header_pair(
            "access-control-expose-headers",
            &cors.expose_headers.join(", "),
        ));
    }
    if cors.allow_credentials {
        headers.push(header_pair("access-control-allow-credentials", "true"));
    }
    headers.into_iter().flatten().collect()
}

/// A single header pair, `None` when either side is not a valid header value.
fn header_pair(name: &str, value: &str) -> Option<(HeaderName, HeaderValue)> {
    Some((
        HeaderName::from_bytes(name.as_bytes()).ok()?,
        HeaderValue::from_str(value).ok()?,
    ))
}

/// A request body bounded while it streams.
///
/// A body with a known `Content-Length` was already checked by
/// [`DataPlaneServiceImpl::enforce_payload_rules`]; a chunked body has no
/// declared size, so it is bounded here instead of being able to grow without
/// limit on its way to the upstream. Chunks flow through one at a time; the
/// first chunk that crosses the budget fails the body with
/// [`RequestBodyError::BudgetExceeded`], which the transport turns into a 413.
#[must_use = "the bounded body must replace the original one"]
pub fn limited_body(body: axum::body::Body, budget: u64) -> axum::body::Body {
    if axum::body::HttpBody::size_hint(&body).exact().is_some() {
        return body;
    }
    axum::body::Body::from_stream(LimitedBody {
        inner: body,
        remaining: budget,
    })
}

/// The stream behind [`limited_body`].
struct LimitedBody {
    inner: axum::body::Body,
    remaining: u64,
}

impl futures_util::Stream for LimitedBody {
    type Item = Result<bytes::Bytes, RequestBodyError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match axum::body::HttpBody::poll_frame(std::pin::Pin::new(&mut this.inner), cx) {
                std::task::Poll::Ready(Some(Ok(frame))) => {
                    let Ok(data) = frame.into_data() else {
                        // A trailer frame carries no payload: keep polling.
                        continue;
                    };
                    let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
                    if length > this.remaining {
                        this.remaining = 0;
                        return std::task::Poll::Ready(Some(Err(RequestBodyError::BudgetExceeded)));
                    }
                    this.remaining -= length;
                    return std::task::Poll::Ready(Some(Ok(data)));
                }
                std::task::Poll::Ready(Some(Err(_))) => {
                    return std::task::Poll::Ready(Some(Err(RequestBodyError::Aborted)));
                }
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Structured audit line with allowlisted fields only (DESIGN §4.3).
///
/// `path` is the externally visible path (`/<alias><upstream path>`) and never
/// the internal route key, which is logged separately as `route`; the
/// identifier is the one the request carried, so the line correlates with the
/// `X-Request-ID` the caller received.
fn audit(
    target: Option<&ResolvedTarget>,
    scope: &RequestScope<'_>,
    endpoint: Option<&Endpoint>,
    status: u16,
    elapsed: Duration,
    error: Option<&DomainError>,
) {
    let host = target.map_or_else(
        || scope.alias.to_owned(),
        |resolved| resolved.upstream.alias.clone(),
    );
    let route = target.map_or_else(
        || "unresolved".to_owned(),
        |resolved| resolved.route.match_key(),
    );
    let path = format!("/{}{}", scope.alias, scope.request.path);
    let base = (
        "proxy.request",
        scope.request_id,
        scope.ctx.subject_tenant_id().to_string(),
        scope.ctx.subject_id().to_string(),
        host,
        route,
        path,
        scope.request.method.to_string(),
        status,
        elapsed.as_millis(),
    );
    match (error, endpoint) {
        (Some(error), _) => tracing::info!(
            event = base.0,
            request_id = %base.1,
            tenant_id = %base.2,
            principal_id = %base.3,
            host = %base.4,
            route = %base.5,
            path = %base.6,
            method = %base.7,
            status = base.8,
            duration_ms = base.9,
            error_type = error_metric_type(error),
            "proxy request rejected"
        ),
        (None, Some(endpoint)) => tracing::info!(
            event = base.0,
            request_id = %base.1,
            tenant_id = %base.2,
            principal_id = %base.3,
            host = %base.4,
            route = %base.5,
            path = %base.6,
            method = %base.7,
            endpoint = %endpoint_authority(endpoint),
            status = base.8,
            duration_ms = base.9,
            "proxied request"
        ),
        (None, None) => tracing::info!(
            event = base.0,
            request_id = %base.1,
            tenant_id = %base.2,
            principal_id = %base.3,
            host = %base.4,
            route = %base.5,
            path = %base.6,
            method = %base.7,
            status = base.8,
            duration_ms = base.9,
            "proxied request"
        ),
    }
}

/// A body stream that fails when the upstream goes idle for too long.
///
/// Frames are forwarded one at a time; nothing is buffered, so streaming
/// responses (including SSE) pass through as they arrive.
pub struct IdleTimeoutStream {
    inner: hyper::body::Incoming,
    timeout: Duration,
    idle: std::pin::Pin<Box<tokio::time::Sleep>>,
    done: bool,
}

impl IdleTimeoutStream {
    /// Wraps an upstream body with an idle deadline.
    #[must_use]
    pub fn new(inner: hyper::body::Incoming, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            idle: Box::pin(tokio::time::sleep_until(
                tokio::time::Instant::now() + timeout,
            )),
            done: false,
        }
    }
}

impl futures_util::Stream for IdleTimeoutStream {
    type Item = Result<bytes::Bytes, DomainError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return std::task::Poll::Ready(None);
        }
        match axum::body::HttpBody::poll_frame(std::pin::Pin::new(&mut this.inner), cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                this.idle
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.timeout);
                // Trailer frames close the body without carrying data.
                frame.into_data().map_or_else(
                    |_| std::task::Poll::Ready(None),
                    |chunk| std::task::Poll::Ready(Some(Ok(chunk))),
                )
            }
            std::task::Poll::Ready(Some(Err(_))) => {
                this.done = true;
                std::task::Poll::Ready(Some(Err(DomainError::StreamAborted(
                    "the upstream response body failed".to_owned(),
                ))))
            }
            std::task::Poll::Ready(None) => {
                this.done = true;
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => match this.idle.as_mut().poll(cx) {
                std::task::Poll::Ready(()) => {
                    this.done = true;
                    std::task::Poll::Ready(Some(Err(DomainError::IdleTimeout(
                        "the upstream stopped sending data".to_owned(),
                    ))))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Http,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn the_ssrf_guard_blocks_every_non_routable_class() {
        // The five addresses the review called out, plus their IPv6 forms.
        for host in [
            "::ffff:169.254.169.254",
            "fc00::1",
            "fe80::1",
            "0.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "::1",
            "::",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.18.0.1",
        ] {
            let address: IpAddr = host.parse().unwrap();
            assert!(is_blocked_address(address), "{host} must be blocked");
        }
    }

    #[test]
    fn the_ssrf_guard_still_allows_public_unicast() {
        for host in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            let address: IpAddr = host.parse().unwrap();
            assert!(!is_blocked_address(address), "{host} must be allowed");
        }
    }

    #[test]
    fn ipv4_mapped_ipv6_inherits_the_ipv4_rules() {
        let metadata: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
        let private: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        let public: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(is_blocked_address(metadata));
        assert!(is_blocked_address(private));
        assert!(!is_blocked_address(public));
    }

    #[test]
    fn an_ipv6_endpoint_is_bracketed_in_the_authority_and_the_uri() {
        let v6 = endpoint("2001:db8::1", 8443);
        assert_eq!(endpoint_authority(&v6), "[2001:db8::1]:8443");
        let uri = build_uri("https", "2001:db8::1", 8443, "/v1/feed", "a=1").unwrap();
        assert_eq!(uri.authority().unwrap().as_str(), "[2001:db8::1]:8443");
        assert_eq!(uri.path_and_query().unwrap().as_str(), "/v1/feed?a=1");

        let plain = endpoint("api.vendor.com", 443);
        assert_eq!(endpoint_authority(&plain), "api.vendor.com:443");
        // An already bracketed literal is not bracketed twice.
        assert_eq!(bracketed("[2001:db8::1]"), "[2001:db8::1]");
    }

    #[test]
    fn the_connect_budget_is_bounded_in_both_directions() {
        assert_eq!(connect_budget(0), Duration::from_secs(1));
        assert_eq!(
            connect_budget(30),
            Duration::from_secs(MAX_CONNECT_TIMEOUT_SECS)
        );
        assert_eq!(connect_budget(5), Duration::from_secs(5));
    }

    #[test]
    fn the_connection_header_extends_the_hop_by_hop_set() {
        let connection = HeaderValue::from_static("Keep-Alive, X-Internal-Trace");
        let stripped = hop_by_hop_strip_set(Some(&connection));
        for expected in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
            "x-internal-trace",
        ] {
            assert!(
                stripped.iter().any(|name| name.as_str() == expected),
                "{expected} must be stripped"
            );
        }
        assert_eq!(connection_tokens(None), Vec::<String>::new());
        assert_eq!(
            connection_tokens(Some(&connection)),
            vec!["keep-alive", "x-internal-trace"]
        );
        // A token named by `Connection` is never forwarded, whatever the
        // passthrough rules say.
        assert!(
            hop_by_hop_strip_set(Some(&HeaderValue::from_static("host")))
                .iter()
                .any(|name| name.as_str() == "host")
        );
    }

    #[test]
    fn a_preflight_response_carries_the_cors_headers() {
        let cors = CorsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: vec!["x-request-id".to_owned()],
            allow_credentials: true,
        };
        let mut request = HeaderMap::new();
        request.insert(
            http::header::ACCESS_CONTROL_REQUEST_HEADERS,
            HeaderValue::from_static("x-trace, x-tenant"),
        );
        let headers = preflight_headers(&cors, "https://app.example.com", &request);
        let value = |name: &str| {
            headers
                .iter()
                .find(|(header, _)| header.as_str() == name)
                .map(|(_, value)| value.to_str().unwrap().to_owned())
        };
        assert_eq!(
            value("access-control-allow-origin").as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(
            value("access-control-allow-methods").as_deref(),
            Some("GET, POST")
        );
        assert_eq!(
            value("access-control-allow-headers").as_deref(),
            Some("x-trace, x-tenant")
        );
        assert_eq!(value("access-control-max-age").as_deref(), Some("86400"));
        assert_eq!(
            value("vary").as_deref(),
            Some(crate::domain::cors::PREFLIGHT_VARY)
        );
        assert_eq!(
            value("access-control-allow-credentials").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn an_actual_cross_origin_response_carries_the_cors_headers() {
        let cors = CorsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec!["x-request-id".to_owned(), "x-ratelimit-limit".to_owned()],
            allow_credentials: false,
        };
        let headers = cors_response_headers(&cors, "https://app.example.com");
        let rendered: Vec<(String, String)> = headers
            .into_iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap().to_owned()))
            .collect();
        assert!(rendered.contains(&(
            "access-control-allow-origin".to_owned(),
            "https://app.example.com".to_owned()
        )));
        assert!(rendered.contains(&("vary".to_owned(), "Origin".to_owned())));
        assert!(
            rendered
                .iter()
                .any(|(name, value)| name == "access-control-expose-headers"
                    && value.contains("x-request-id"))
        );
        assert!(
            rendered
                .iter()
                .all(|(name, _)| name != "access-control-allow-credentials")
        );
    }

    #[test]
    fn the_rate_limit_headers_are_stamped_from_the_decision() {
        let decision = RateDecision::Allowed {
            limit: 10,
            remaining: 7,
            reset_seconds: 3,
        };
        let headers = RateLimitHeaders::of(&decision);
        let mut map = HeaderMap::new();
        headers.apply(&mut map);
        assert_eq!(map.get("x-ratelimit-limit").unwrap(), "10");
        assert_eq!(map.get("x-ratelimit-remaining").unwrap(), "7");
        assert_eq!(map.get("x-ratelimit-reset").unwrap(), "3");
    }

    #[test]
    fn forwarding_headers_are_ignored_without_a_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
        headers.insert("forwarded", HeaderValue::from_static("for=203.0.113.7"));
        assert!(forwarded_client_ip(&headers, &[]).is_none());
    }

    #[test]
    fn a_trusted_proxy_list_lets_the_first_hop_be_believed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.9"),
        );
        let trusted = vec![
            TrustedProxy::parse("10.0.0.0/8").unwrap(),
            TrustedProxy::parse("fd00::/8").unwrap(),
        ];
        assert_eq!(
            forwarded_client_ip(&headers, &trusted),
            "203.0.113.7".parse::<IpAddr>().ok()
        );
        // A bare proxy address is parsed too.
        let bare = vec![TrustedProxy::parse("10.0.0.9").unwrap()];
        assert!(forwarded_client_ip(&headers, &bare).is_some());
    }

    #[test]
    fn forwarding_header_junk_never_yields_a_client_ip() {
        let trusted = vec![TrustedProxy::parse("10.0.0.0/8").unwrap()];
        for value in [
            "for=203.0.113.7",
            "for=203.0.113.7;by=10.0.0.9",
            "not-an-address",
            "[2001:db8::1",
            "",
            "  ",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", HeaderValue::from_str(value).unwrap());
            assert!(forwarded_client_ip(&headers, &trusted).is_none(), "{value}");
        }
    }

    #[test]
    fn a_trusted_proxy_range_is_respected() {
        let proxy = TrustedProxy::parse("10.0.0.0/8").unwrap();
        assert!(proxy.covers("10.1.2.3".parse().unwrap()));
        assert!(!proxy.covers("11.0.0.1".parse().unwrap()));
        let v6 = TrustedProxy::parse("fd00::/8").unwrap();
        assert!(v6.covers("fd12:3456::1".parse().unwrap()));
        assert!(!v6.covers("fe80::1".parse().unwrap()));
        // An invalid prefix is refused, never silently widened.
        assert!(TrustedProxy::parse("10.0.0.0/33").is_none());
        assert!(TrustedProxy::parse("fd00::/129").is_none());
    }

    #[tokio::test]
    async fn a_chunked_body_is_cut_off_at_the_payload_budget() {
        let chunks: Vec<Result<bytes::Bytes, std::convert::Infallible>> = (0..8)
            .map(|_| Ok(bytes::Bytes::from_static(b"0123456789")))
            .collect();
        let body = axum::body::Body::from_stream(futures_util::stream::iter(chunks));
        let limited = limited_body(body, 25);
        let collected = http_body_util::BodyExt::collect(limited).await;
        assert!(collected.is_err(), "the body must fail past the budget");

        // A body with a declared length is passed through untouched and is
        // bounded by the content-length check instead.
        let exact = limited_body(axum::body::Body::from("0123456789"), 5);
        let collected = http_body_util::BodyExt::collect(exact).await.unwrap();
        assert_eq!(collected.to_bytes(), &b"0123456789"[..]);
    }

    #[tokio::test]
    async fn a_body_within_the_budget_streams_untouched() {
        let chunks: Vec<Result<bytes::Bytes, std::convert::Infallible>> = vec![
            Ok(bytes::Bytes::from_static(b"chunk-")),
            Ok(bytes::Bytes::from_static(b"one")),
        ];
        let body = axum::body::Body::from_stream(futures_util::stream::iter(chunks));
        let limited = limited_body(body, MAX_PAYLOAD_BYTES);
        let collected = http_body_util::BodyExt::collect(limited).await.unwrap();
        assert_eq!(collected.to_bytes(), &b"chunk-one"[..]);
    }
}
