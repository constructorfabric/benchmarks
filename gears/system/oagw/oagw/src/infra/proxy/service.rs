//! The data-plane implementation of the [`DataPlane`] contract.
//!
//! The service owns the whole request lifecycle: alias walk, route match,
//! configuration merge, endpoint selection, plugin chain, rate limiting, the
//! outbound call and the response transformation. Every failure it produces
//! carries the error-source marker of a gateway-produced failure.

use crate::domain::error::OagwError;
use crate::domain::headers as header_rules;
use crate::domain::merge;
use crate::domain::model::{Endpoint, HeadersConfig, Route, Upstream};
use crate::domain::model::{RateAlgorithm, RateLimit, RateScope, RateStrategy, RateWindow};
use crate::domain::plugin::registry::AuthPluginRegistry;
use crate::domain::plugin::registry::{self, GuardPluginRegistry, TransformPluginRegistry};
use crate::domain::plugin::{
    AuthDecision, CredentialResolver, GuardDecision, PluginRequestContext, PluginResponseContext,
};
use crate::domain::ratelimit::{BucketKey, RateDecision, RateLimiter, rate_limit_headers};
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::domain::services::proxy::{
    DataPlane, ProxyBody, ProxyRequest, ProxySuccess, ResolvedProxy, UpstreamTunnel,
};
use crate::domain::{alias, match_route, validation};
use crate::infra::metrics::Metrics;
use crate::infra::proxy::outbound::{OutboundClient, add_header};
use crate::infra::proxy::{sse, websocket};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Hard limit on a proxied request body.
pub const MAX_REQUEST_BODY_BYTES: usize = 100 * 1024 * 1024;

/// The configured, ready-to-serve data plane.
pub struct DataPlaneServiceImpl {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    credentials: Arc<dyn CredentialResolver>,
    auth_plugins: AuthPluginRegistry,
    guard_plugins: GuardPluginRegistry,
    transform_plugins: TransformPluginRegistry,
    limiter: Arc<RateLimiter>,
    outbound: OutboundClient,
    metrics: Arc<Metrics>,
    round_robin: AtomicU64,
}

impl std::fmt::Debug for DataPlaneServiceImpl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DataPlaneServiceImpl")
            .field("outbound", &self.outbound)
            .finish_non_exhaustive()
    }
}

/// Whether an upstream's alias is the common suffix its endpoint pool shares,
/// which is the case ADR-0001 makes the target-host header required for.
fn upstream_is_common_suffix_pool(upstream: &Upstream) -> bool {
    matches!(
        alias::derive(&upstream.server.endpoints),
        alias::DerivedAlias::Derived(suffix) if suffix == alias::normalise(&upstream.alias)
    )
}

#[allow(clippy::too_many_arguments)]
impl DataPlaneServiceImpl {
    /// Assembles the data plane over its collaborators.
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        credentials: Arc<dyn CredentialResolver>,
        auth_plugins: AuthPluginRegistry,
        guard_plugins: GuardPluginRegistry,
        transform_plugins: TransformPluginRegistry,
        limiter: Arc<RateLimiter>,
        outbound: OutboundClient,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            credentials,
            auth_plugins,
            guard_plugins,
            transform_plugins,
            limiter,
            outbound,
            metrics,
            round_robin: AtomicU64::new(0),
        }
    }

    /// The shared rate-limit registry, so the gear can expose bucket state.
    #[must_use]
    pub fn limiter(&self) -> &Arc<RateLimiter> {
        &self.limiter
    }

    /// The outbound leg, so the gear can report its configuration.
    #[must_use]
    pub fn outbound(&self) -> &OutboundClient {
        &self.outbound
    }

    /// Resolves the upstream an alias names, walking the tenant chain.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when no enabled upstream in the
    /// chain owns the alias and [`OagwError::LinkUnavailable`] when the closest
    /// one is disabled.
    pub async fn resolve_upstream(
        &self,
        tenant_id: Uuid,
        alias_value: &str,
    ) -> Result<Upstream, OagwError> {
        let wanted = alias::normalise(alias_value);
        for candidate in tenant_chain(tenant_id) {
            if let Ok(upstream) = self.upstreams.find_by_alias(candidate, &wanted).await {
                if !upstream.enabled {
                    return Err(OagwError::LinkUnavailable(format!(
                        "upstream '{wanted}' is disabled"
                    )));
                }
                return Ok(upstream);
            }
        }
        Err(OagwError::RouteNotFound(format!(
            "no upstream answers to alias '{wanted}'"
        )))
    }

    /// Selects an endpoint, honouring an explicit target host.
    ///
    /// A pool whose alias is the endpoints' common suffix cannot name an
    /// endpoint on its own, so it requires the `X-OAGW-Target-Host` header; a
    /// pool reached through an explicit alias keeps the round-robin.
    ///
    /// # Errors
    ///
    /// Returns the target-host validation errors documented in
    /// `contracts/errors.md`.
    pub fn select_endpoint(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<Endpoint, OagwError> {
        let endpoints = &upstream.server.endpoints;
        if let Some(host) = target_host.map(str::trim).filter(|value| !value.is_empty()) {
            // A value that is not a bare hostname or IP is malformed, not
            // merely unknown: it is rejected before the lookup.
            if !validation::is_valid_host(host) {
                return Err(OagwError::InvalidTargetHost(format!(
                    "'{host}' is not a bare hostname or IP address"
                )));
            }
            let normalised = host.trim_end_matches('.').to_ascii_lowercase();
            return endpoints
                .iter()
                .find(|endpoint| {
                    endpoint.normalised_host() == normalised || endpoint.authority() == normalised
                })
                .cloned()
                .ok_or_else(|| {
                    OagwError::UnknownTargetHost(format!(
                        "host '{normalised}' is not an endpoint of this upstream"
                    ))
                });
        }
        if endpoints.len() > 1 && upstream_is_common_suffix_pool(upstream) {
            let hosts = endpoints
                .iter()
                .map(crate::domain::model::Endpoint::normalised_host)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(OagwError::MissingTargetHost(format!(
                "alias '{}' spans several endpoints; set X-OAGW-Target-Host to one of: {hosts}",
                upstream.alias
            )));
        }
        let tick = usize::try_from(self.round_robin.fetch_add(1, Ordering::Relaxed)).unwrap_or(0);
        let index = if endpoints.len() == 1 {
            0
        } else {
            tick % endpoints.len()
        };
        Ok(endpoints[index].clone())
    }

    /// Runs the rate-limit check for a merged configuration.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RateLimitExceeded`] when the bucket is empty.
    pub fn apply_rate_limit(
        &self,
        request: &ProxyRequest,
        effective: &merge::EffectiveConfig,
        resource: &str,
    ) -> Result<Option<RateDecision>, OagwError> {
        let Some(rate) = effective.sustained_rate else {
            return Ok(None);
        };
        let limit = RateLimit {
            sharing: crate::domain::model::SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: crate::domain::model::SustainedRate {
                rate,
                window: effective.sustained_window.unwrap_or(RateWindow::Second),
            },
            burst: crate::domain::model::Burst {
                capacity: effective.burst_capacity,
            },
            scope: effective.rate_scope.unwrap_or(RateScope::Tenant),
            strategy: RateStrategy::Reject,
            cost: effective.rate_cost.max(1),
        };
        let key = bucket_key(effective.rate_scope, resource, request);
        let decision = self.limiter.check(&key, &limit, limit.cost, Instant::now());
        if decision.allowed {
            Ok(Some(decision))
        } else {
            Err(RateLimiter::exceeded(&decision))
        }
    }

    async fn run_request_plugins(
        &self,
        context: &mut PluginRequestContext,
        upstream: &Upstream,
        effective: &merge::EffectiveConfig,
    ) -> Result<(), OagwError> {
        // Auth first: it may inject a credential into the outbound request.
        if let Some(auth) = upstream.auth.as_ref() {
            let plugin_id = registry::auth_plugin_id(auth);
            let plugin = self.auth_plugins.get(&plugin_id)?;
            match plugin
                .authenticate(context, auth, self.credentials.as_ref())
                .await?
            {
                AuthDecision::Injected | AuthDecision::Passthrough => {}
            }
        }
        // Then guards and request transforms, in configured order.
        for binding in &effective.plugins {
            if let Ok(guard) = self.guard_plugins.get(&binding.id) {
                if let GuardDecision::Reject { code, detail, .. } =
                    guard.guard_request(context, &binding.config).await?
                {
                    return Err(reject_error(code, detail));
                }
                continue;
            }
            if let Ok(transform) = self.transform_plugins.get(&binding.id) {
                transform
                    .transform_request(context, &binding.config)
                    .await?;
            }
        }
        Ok(())
    }

    async fn run_response_plugins(
        &self,
        context: &mut PluginResponseContext,
        effective: &merge::EffectiveConfig,
        failed: bool,
    ) -> Result<(), OagwError> {
        for binding in &effective.plugins {
            if let Ok(guard) = self.guard_plugins.get(&binding.id) {
                if let GuardDecision::Reject { code, detail, .. } =
                    guard.guard_response(context, &binding.config).await?
                {
                    return Err(reject_error(code, detail));
                }
                continue;
            }
            if let Ok(transform) = self.transform_plugins.get(&binding.id) {
                if failed {
                    transform.transform_error(context, &binding.config).await?;
                } else {
                    transform
                        .transform_response(context, &binding.config)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// The route a request matches, with the path it leaves over.
    ///
    /// The leftover suffix is what the upstream path is built from: the route
    /// prefix is a prefix of the proxy path, and the remainder follows it.
    async fn matched_route(
        &self,
        tenant_id: Uuid,
        upstream: &Upstream,
        method: &str,
        path: &str,
    ) -> Result<(Route, String), OagwError> {
        let routes = self
            .routes
            .list_by_upstream(tenant_id, &upstream.id)
            .await?;
        let matching = match_path(path);
        let Some((route, suffix)) = match_route::select(routes.as_slice(), method, &matching)
        else {
            return Err(OagwError::RouteNotFound(format!(
                "no route on upstream '{}' matches {method} {path}",
                upstream.alias
            )));
        };
        Ok((route.clone(), suffix))
    }

    async fn dispatch_upstream(
        &self,
        context: &PluginRequestContext,
        endpoint: &Endpoint,
    ) -> Result<crate::infra::proxy::outbound::UpstreamResponse, OagwError> {
        let scheme = endpoint.scheme.as_wire_scheme();
        self.outbound.check_scheme(scheme)?;
        let wire_request = crate::infra::proxy::outbound::build_request(
            &context.method,
            scheme,
            &context.target_host,
            endpoint.port,
            &context.path,
            Some(&context.query),
            &header_pairs(&context.headers),
            context.body.clone(),
        )?;
        self.outbound.send(wire_request).await
    }
}

/// Assembles the plugin context the request plugins run against.
///
/// # Errors
///
/// Returns [`OagwError::ValidationError`] when a configured header rule
/// cannot be expressed on the wire.
fn build_context(
    request: &ProxyRequest,
    upstream: &Upstream,
    route: Option<&Route>,
    endpoint: &Endpoint,
    remainder: &str,
) -> Result<PluginRequestContext, OagwError> {
    let merged = merged_headers(upstream, route);
    let mut headers =
        header_rules::build_request_headers(&request.headers, &merged, &merged.request)
            .map_err(|error| OagwError::ValidationError(error.to_string()))?;
    headers.remove("host");
    headers.remove("content-length");
    headers.remove("x-oagw-target-host");
    set_header(&mut headers, "host", &endpoint.authority());
    // A tunnel's handshake headers are not part of the proxied payload: the
    // transformation rules govern what the upstream sees in the body of an
    // ordinary request, while the upgrade itself belongs to the connection.
    // They are restored here so `open_tunnel`, which forwards exactly these,
    // can hand the upstream a handshake the client actually offered.
    if request.is_upgrade {
        for name in websocket::UPGRADE_REQUEST_HEADERS {
            if let Some(value) = request
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
            {
                set_header(&mut headers, name, value);
            }
        }
    }
    set_header(&mut headers, "x-request-id", &request.request_id);
    let prefix = route
        .and_then(|route| route.http_match())
        .map(|match_config| match_config.path.clone())
        .unwrap_or_default();
    Ok(PluginRequestContext {
        request_id: request.request_id.clone(),
        tenant_id: request.tenant_id,
        upstream_id: upstream.id.clone(),
        route_id: route.map(|route| route.id.clone()),
        alias: request.alias.clone(),
        target_host: endpoint.normalised_host(),
        method: request.method.clone(),
        path: outbound_path(&prefix, remainder),
        query: request.query.clone(),
        headers,
        body: request.body.clone(),
        credential: None,
    })
}

/// The headers a proxied response carries back to the client.
///
/// Hop-by-hop headers are stripped, the configured response rules are applied,
/// an event stream is decorated so intermediaries do not buffer it, the
/// rate-limit bucket is advertised and the gateway's own markers are stamped.
/// The error-source marker names the upstream: every byte in this response,
/// whatever its status, came from it.
///
/// # Errors
///
/// Returns [`OagwError::ValidationError`] when a configured response header
/// rule cannot be expressed on the wire.
pub(crate) fn proxied_response_headers(
    upstream_headers: &[(String, String)],
    merged: &HeadersConfig,
    is_stream: bool,
    decision: Option<&RateDecision>,
    request_id: &str,
) -> Result<Vec<(String, String)>, OagwError> {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream_headers {
        add_header(&mut headers, name, value);
    }
    header_rules::strip_hop_by_hop(&mut headers);
    let mut transformed = header_rules::build_response_headers(&headers, &merged.response)
        .map_err(|error| OagwError::ValidationError(error.to_string()))?;
    if is_stream {
        for (name, value) in sse::decoration_for(upstream_headers) {
            add_header(&mut transformed, &name, &value);
        }
    }
    let mut pairs = header_pairs(&transformed);
    if let Some(decision) = decision {
        for (name, value) in rate_limit_headers(decision) {
            pairs.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
            pairs.push((name, value));
        }
    }
    replace_header(&mut pairs, "X-OAGW-Error-Source", "upstream");
    replace_header(&mut pairs, "X-Request-ID", request_id);
    Ok(pairs)
}

/// The rate-limit bucket key a scope and request resolve to.
fn bucket_key(scope: Option<RateScope>, resource: &str, request: &ProxyRequest) -> BucketKey {
    let scope_name = match scope.unwrap_or(RateScope::Tenant) {
        RateScope::Global => "global",
        RateScope::Tenant => "tenant",
        RateScope::User => "user",
        RateScope::Ip => "ip",
        RateScope::Route => "route",
    };
    BucketKey::new(scope_name, resource.to_owned(), principal_of(request))
}

/// The merged header rules for an upstream/route pair.
#[must_use]
pub fn merged_headers(upstream: &Upstream, _route: Option<&Route>) -> HeadersConfig {
    upstream.headers.clone()
}

/// The upstream path for a matched route and the remaining suffix.
#[must_use]
pub fn outbound_path(prefix: &str, suffix: &str) -> String {
    let base = prefix.trim_end_matches('/');
    if suffix.is_empty() {
        if base.is_empty() {
            "/".to_owned()
        } else {
            base.to_owned()
        }
    } else if base.is_empty() {
        format!("/{suffix}")
    } else {
        format!("{base}/{suffix}")
    }
}

/// The request path the route matcher sees, always slash-prefixed.
///
/// The proxy hands over the path that followed the alias, without its
/// separator, so `v1/chat` and the route prefix `/v1/chat` describe the same
/// address only once the leading slash is restored.
#[must_use]
fn match_path(suffix: &str) -> String {
    if suffix.starts_with('/') {
        suffix.to_owned()
    } else {
        format!("/{suffix}")
    }
}

#[async_trait::async_trait]
impl DataPlane for DataPlaneServiceImpl {
    async fn handle(&self, request: ProxyRequest) -> Result<ProxySuccess, OagwError> {
        self.metrics.proxy_started();
        let upstream = self
            .resolve_upstream(request.tenant_id, &request.alias)
            .await?;
        if request.body.len() > MAX_REQUEST_BODY_BYTES {
            self.metrics.rejected("payload_too_large");
            return Err(OagwError::PayloadTooLarge(format!(
                "request body of {} bytes exceeds the {} byte limit",
                request.body.len(),
                MAX_REQUEST_BODY_BYTES
            )));
        }
        let (route, remainder) = self
            .matched_route(
                request.tenant_id,
                &upstream,
                &request.method,
                &request.path_suffix,
            )
            .await
            .inspect_err(|_| self.metrics.rejected("route_not_found"))?;
        let effective = merge::effective(&upstream, Some(&route));
        let endpoint = self.select_endpoint(&upstream, request.target_host.as_deref())?;
        let decision = self.apply_rate_limit(&request, &effective, &route.id)?;
        let mut context = build_context(&request, &upstream, Some(&route), &endpoint, &remainder)?;
        self.run_request_plugins(&mut context, &upstream, &effective)
            .await?;

        let response = self.dispatch_upstream(&context, &endpoint).await?;
        let is_stream = response.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type") && sse::is_event_stream(value)
        });

        // Response headers: strip hop-by-hop, apply the configured rules, then
        // decorate with the gateway's own headers.
        let merged = merged_headers(&upstream, Some(&route));
        let mut pairs = proxied_response_headers(
            &response.headers,
            &merged,
            is_stream,
            decision.as_ref(),
            &request.request_id,
        )?;

        let mut response_context = PluginResponseContext {
            request_id: request.request_id.clone(),
            status: response.status,
            headers: {
                let mut headers = HeaderMap::new();
                for (name, value) in &pairs {
                    add_header(&mut headers, name, value);
                }
                headers
            },
        };
        self.run_response_plugins(&mut response_context, &effective, false)
            .await?;
        for (name, value) in header_pairs(&response_context.headers) {
            replace_header(&mut pairs, &name, &value);
        }
        self.metrics.upstream_status(response.status);
        Ok(ProxySuccess {
            status: response.status,
            headers: pairs,
            body: ProxyBody::Streaming(response.body),
        })
    }

    async fn resolve(&self, request: ProxyRequest) -> Result<ResolvedProxy, OagwError> {
        let upstream = self
            .resolve_upstream(request.tenant_id, &request.alias)
            .await?;
        let (route, remainder) = self
            .matched_route(
                request.tenant_id,
                &upstream,
                &negotiated_method(&request),
                &request.path_suffix,
            )
            .await?;
        let effective = merge::effective(&upstream, Some(&route));
        let endpoint = self.select_endpoint(&upstream, request.target_host.as_deref())?;
        let mut context = build_context(&request, &upstream, Some(&route), &endpoint, &remainder)?;
        // A preflight is answered before any per-request policy runs: it asks
        // about a request that has not happened yet, so neither the credentials
        // nor the guards of that future request can be consulted.
        if !request.is_preflight {
            self.run_request_plugins(&mut context, &upstream, &effective)
                .await?;
        }
        Ok(ResolvedProxy {
            request_id: request.request_id,
            scheme: endpoint.scheme.as_wire_scheme().to_owned(),
            host: context.target_host,
            port: endpoint.port,
            path: context.path,
            headers: header_pairs(&context.headers),
            effective,
        })
    }

    async fn open_tunnel(&self, request: ProxyRequest) -> Result<UpstreamTunnel, OagwError> {
        let resolved = self.resolve(request).await?;
        let headers: Vec<(String, String)> = resolved
            .headers
            .iter()
            .filter(|(name, _)| {
                websocket::UPGRADE_REQUEST_HEADERS
                    .iter()
                    .any(|kept| kept.eq_ignore_ascii_case(name))
            })
            .cloned()
            .collect();
        self.outbound.check_scheme(&resolved.scheme)?;
        let wire_request = crate::infra::proxy::outbound::build_request(
            "GET",
            &resolved.scheme,
            &resolved.host,
            resolved.port,
            &resolved.path,
            None,
            &headers,
            Bytes::new(),
        )?;
        let tunnel = self.outbound.connect(wire_request).await?;
        Ok(UpstreamTunnel {
            status: tunnel.status,
            headers: websocket::response_handshake_headers(&tunnel.headers),
            stream: tunnel.stream,
        })
    }
}

/// The tenant chain walked for an alias, closest first.
#[must_use]
pub fn tenant_chain(tenant_id: Uuid) -> Vec<Uuid> {
    if tenant_id == Uuid::nil() {
        vec![tenant_id]
    } else {
        vec![tenant_id, Uuid::nil()]
    }
}

/// The method a route is matched against.
///
/// A preflight asks about the request the browser is about to send, so the
/// route is selected by the method named in `Access-Control-Request-Method`
/// rather than by `OPTIONS`, which no route lists.
#[must_use]
fn negotiated_method(request: &ProxyRequest) -> String {
    if !request.is_preflight {
        return request.method.clone();
    }
    let requested = request
        .headers
        .get(http::header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    requested.map_or_else(|| request.method.clone(), str::to_owned)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

/// The rate-limit principal named by a request.
#[must_use]
pub fn principal_of(request: &ProxyRequest) -> String {
    let credential = request
        .headers
        .get("authorization")
        .or_else(|| request.headers.get("x-api-key"))
        .and_then(|value| value.to_str().ok());
    match credential {
        Some(value) => format!("key:{:016x}", digest(value.as_bytes())),
        None => format!("tenant:{}", request.tenant_id),
    }
}

/// A cheap, stable, non-cryptographic digest for principal naming.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
fn digest(bytes: &[u8]) -> u64 {
    // FNV-1a: stable across restarts and dependencies; never used for security.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Maps a guard rejection code onto the documented gateway error.
///
/// Every guard code is a request the caller got wrong, so they all surface as
/// a validation error; the code only chooses the message.
fn reject_error(code: &str, detail: String) -> OagwError {
    let _ = code;
    OagwError::ValidationError(detail)
}

/// Flattens a header map into ordered wire pairs.
#[must_use]
pub fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
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

fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    let Ok(parsed) = HeaderValue::from_str(value) else {
        return;
    };
    headers.insert(name, parsed);
}

fn replace_header(pairs: &mut Vec<(String, String)>, name: &str, value: &str) {
    pairs.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
    pairs.push((name.to_owned(), value.to_owned()));
}

/// Timeout used when the configuration does not name one.
#[must_use]
pub fn default_proxy_timeout() -> Duration {
    Duration::from_secs(10)
}
