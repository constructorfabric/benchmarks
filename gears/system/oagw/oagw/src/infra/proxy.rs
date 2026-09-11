// Created: 2026-09-02 by Constructor Tech
//! The data plane: alias resolution, route matching, plugin execution and
//! upstream relay (`DESIGN.md` §3.3 Proxy API, §3.5 Proxy Request Flow).
//!
//! The flow is the one the sequence diagram prescribes:
//!
//! ```text
//! resolve_upstream → resolve_route → validate → rate-limit → auth →
//! guards → transform_request → upstream → transform_response → relay
//! ```
//!
//! Relay is pass-through: the body is streamed frame by frame, so
//! server-sent events and other unbounded responses are never buffered, and a
//! `101 Switching Protocols` upgrade hands the socket over with
//! [`tokio::io::copy_bidirectional`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use http::uri::{Authority, Scheme as HttpScheme};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::model::{
    compute_derived_alias, Endpoint, HttpMatch, HttpMethod, PluginsConfig, Protocol,
    RateLimitConfig, RateStrategy, Sharing, SuffixMode, Upstream,
};
use crate::domain::plugin::{
    AuthContext, AuthPluginRegistry, GuardPluginRegistry, GuardVerdict, RequestHead,
    TransformPluginRegistry,
};
use crate::domain::service::ControlPlane;
use crate::domain::store::{parse_resource_id, route_gts_id, upstream_gts_id};
use crate::error::{GatewayError, TimeoutKind};
use crate::gts;
use crate::infra::client::{ancestor_chain, UpstreamClient};
use crate::infra::cors;
use crate::infra::headers as header_rules;
use crate::infra::ratelimit::{effective_limit, RateDecision, RateLimiter};

/// Everything the data plane needs, shared by the whole gear.
pub struct ProxyService {
    /// Control plane: the source of upstream and route configuration.
    pub control_plane: Arc<ControlPlane>,
    /// Upstream transport.
    pub client: UpstreamClient,
    /// The token buckets.
    pub limiter: RateLimiter,
    /// Built-in auth plugins.
    pub auth: AuthPluginRegistry,
    /// Built-in guard plugins.
    pub guards: GuardPluginRegistry,
    /// Built-in transform plugins.
    pub transforms: TransformPluginRegistry,
    /// Gear configuration.
    pub config: OagwConfig,
    /// Round-robin cursor over multi-endpoint pools.
    cursor: AtomicU64,
}

/// Takes the pending protocol switch out of the request extensions.
///
/// Hyper parks the upgrade future there when the client sent
/// `Connection: Upgrade`; a handler must take it before the body is consumed.
#[must_use]
pub fn upgrade_of(parts: &mut axum::http::request::Parts) -> Option<hyper::upgrade::OnUpgrade> {
    parts.extensions.remove::<hyper::upgrade::OnUpgrade>()
}

/// The parts of an inbound proxy request the data plane needs.
pub struct ProxyRequest {
    /// The alias from the path.
    pub alias: String,
    /// The path suffix after the alias, empty when the proxy path ends there.
    pub path_suffix: String,
    /// The raw query string, without the leading `?`.
    pub query: Option<String>,
    /// The inbound method.
    pub method: Method,
    /// The inbound headers.
    pub headers: HeaderMap,
    /// The inbound body.
    pub body: Body,
    /// The calling tenant.
    pub tenant_id: Uuid,
    /// The calling subject, for user-scoped rate limits.
    pub subject_id: String,
    /// The client IP, for IP-scoped rate limits.
    pub client_ip: String,
    /// Pending server-side upgrade, captured by the handler when the client
    /// asked to switch protocols.
    pub upgrade: Option<hyper::upgrade::OnUpgrade>,
}

/// The caller's identity, owned and cheap to clone.
///
/// Carried separately from [`ProxyRequest`] so the plugin and rate-limit
/// helpers never hold a reference to a request whose body is not `Sync`
/// across an `await`.
#[derive(Clone, Debug)]
struct CallContext {
    tenant_id: Uuid,
    subject_id: String,
    client_ip: String,
}

impl ProxyService {
    /// Builds the service from its collaborators.
    #[must_use]
    pub fn new(
        control_plane: Arc<ControlPlane>,
        client: UpstreamClient,
        auth: AuthPluginRegistry,
        guards: GuardPluginRegistry,
        transforms: TransformPluginRegistry,
        config: OagwConfig,
    ) -> Self {
        Self {
            control_plane,
            client,
            limiter: RateLimiter::new(),
            auth,
            guards,
            transforms,
            config,
            cursor: AtomicU64::new(0),
        }
    }

    /// Executes a proxied request end to end.
    ///
    /// # Errors
    ///
    /// Returns the [`GatewayError`] to render when the request cannot be
    /// served; a successful return is the upstream response relayed verbatim.
    pub async fn execute(&self, request: ProxyRequest) -> Result<Response, GatewayError> {
        let mut request = request;
        let caller = CallContext {
            tenant_id: request.tenant_id,
            subject_id: request.subject_id.clone(),
            client_ip: request.client_ip.clone(),
        };
        let started = std::time::Instant::now();
        let chain = ancestor_chain(self.control_plane.resolver().as_ref(), request.tenant_id).await;

        let (upstream_id, upstream) = self.resolve_upstream(&request.alias, &chain)?;
        if !upstream.enabled {
            return Err(GatewayError::LinkUnavailable(format!(
                "upstream {} is disabled",
                upstream.alias
            )));
        }

        let suffix = normalize_suffix(&request.path_suffix);
        let route = self.resolve_route(upstream_id, &upstream, &request.method, &suffix)?;
        let http_match = route
            .as_ref()
            .and_then(|(_, spec)| spec.match_config.http.clone())
            .unwrap_or_default();

        // Guards that need no upstream connection run first.
        validate_method(&http_match, &request.method)?;
        validate_query(&http_match, request.query.as_deref())?;
        validate_suffix(&http_match, &suffix)?;

        let target = self.select_endpoint(upstream_id, &upstream, &request.headers)?;
        self.client.screen_host(&target.normalized_host())?;
        ensure_permitted_scheme(&target, self.client.allows_http())?;

        let body = self
            .read_body(std::mem::replace(&mut request.body, Body::empty()), &request.headers)
            .await?;

        // CORS on the actual cross-origin request, after upstream resolution.
        let cors_headers =
            cors::check_request(&request.headers, &request.method, upstream.cors.as_ref())?;

        self.enforce_rate_limit(&upstream, route.as_ref().map(|(_, r)| r), &caller, &chain)
            .await?;

        // ---- plugin chain: upstream bindings, then route bindings ----
        let bindings = collect_plugins(&upstream, route.as_ref().map(|(_, r)| r));
        let injections = self.authenticate(&bindings, &caller, upstream_id).await?;
        self.guard_request(&bindings, &request.headers)?;

        let target_authority = authority(&target);
        let path = upstream_path(&http_match, &suffix);
        let query = outbound_query(
            request.query.as_deref(),
            &http_match,
            &injections.query,
        );
        let request_id = request_id_header(&request.headers);

        let mut outbound = header_rules::outbound_request_headers(
            &request.headers,
            upstream.headers.as_ref().and_then(|h| h.request.as_ref()),
            &injections.headers,
            &target_authority,
            request_id.as_ref(),
        );
        if let Some(protocol) = request.upgrade_protocol() {
            // The hop-by-hop strip must not cost us the protocol switch.
            if let Ok(value) = HeaderValue::from_str(&protocol) {
                outbound.insert(header::UPGRADE, value);
            }
            outbound.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
            copy_upgrade_negotiation(&request.headers, &mut outbound);
        }

        // Transforms run after the header rules, immediately before the dial.
        let mut head = RequestHead {
            path: path.clone(),
            query: query.clone(),
            headers: outbound.clone(),
        };
        for binding in &bindings {
            if let Some(plugin) = self.transforms.get(&binding.plugin_ref) {
                plugin.transform_request(&mut head, &binding.config_map());
            }
        }
        outbound = head.headers;

        let response = self
            .dispatch(
                &upstream,
                request.method.clone(),
                &target,
                &path,
                &query,
                outbound,
                body,
                cors_headers.unwrap_or_default(),
                &bindings,
                request.upgrade,
            )
            .await?;

        let elapsed = started.elapsed();
        tracing::info!(
            alias = %upstream.alias,
            status = %response.status().as_u16(),
            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "proxied request"
        );
        Ok(response)
    }

    /// Finds the closest enabled upstream by alias, walking descendant → root.
    fn resolve_upstream(
        &self,
        alias: &str,
        chain: &[Uuid],
    ) -> Result<(Uuid, Upstream), GatewayError> {
        self.control_plane
            .store()
            .upstream_with_alias(chain, alias)
            .map(|record| (record.id, record.spec))
            .ok_or_else(|| {
                GatewayError::NotFound(format!("no upstream with alias {alias:?} is reachable"))
            })
    }

    /// The longest-prefix route for `(upstream, method, suffix)`.
    ///
    /// An upstream with no routes at all serves every path; one with routes
    /// restricts the proxy to them and answers `404` otherwise.
    fn resolve_route(
        &self,
        upstream_id: Uuid,
        upstream: &Upstream,
        method: &Method,
        suffix: &str,
    ) -> Result<Option<(Uuid, crate::domain::model::Route)>, GatewayError> {
        if upstream.protocol != Protocol::Http {
            return Err(GatewayError::ProtocolError(format!(
                "protocol {:?} has no proxy path in this release",
                upstream.protocol
            )));
        }
        let routes = self.control_plane.store().routes_for_upstream(upstream_id);
        if routes.is_empty() {
            return Ok(None);
        }
        let best = routes
            .iter()
            .filter(|record| {
                record
                    .spec
                    .match_config
                    .http
                    .as_ref()
                    .is_some_and(|m| method_allowed_by(m, method) && path_matches(m, suffix))
            })
            .max_by_key(|record| record.spec.match_config.http.as_ref().unwrap().path.len());
        match best {
            Some(record) => Ok(Some((record.id, record.spec.clone()))),
            None => Err(GatewayError::RouteNotFound(format!(
                "no route of {} accepts {} {}",
                upstream.alias,
                method.as_str(),
                suffix
            ))),
        }
    }

    /// Picks the endpoint to dial: the caller's choice, or the pool's.
    fn select_endpoint(
        &self,
        upstream_id: Uuid,
        upstream: &Upstream,
        headers: &HeaderMap,
    ) -> Result<Endpoint, GatewayError> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(GatewayError::LinkUnavailable(
                "upstream has no endpoints".to_owned(),
            ));
        }
        if endpoints.len() == 1 {
            return Ok(endpoints[0].clone());
        }
        let hosts: Vec<String> = endpoints.iter().map(Endpoint::host_port).collect();
        match target_host(headers) {
            Some(raw) => {
                let requested = raw.trim().to_ascii_lowercase();
                let bare = !requested.contains('/') && !requested.contains(' ');
                if !bare {
                    return Err(GatewayError::InvalidTargetHost {
                        upstream_id: upstream_gts_id(upstream_id),
                        invalid_value: raw.to_owned(),
                    });
                }
                endpoints
                    .iter()
                    .find(|e| e.normalized_host() == requested || e.host_port() == requested)
                    .cloned()
                    .ok_or_else(|| GatewayError::UnknownTargetHost {
                        upstream_id: upstream_gts_id(upstream_id),
                        invalid_value: raw.to_owned(),
                        valid_hosts: hosts,
                    })
            }
            // A pool whose alias is its common suffix needs the header to say
            // which member serves the request (ADR-0001).
            None if is_common_suffix_pool(upstream) => Err(GatewayError::MissingTargetHost {
                upstream_id: upstream_gts_id(upstream_id),
                alias: upstream.alias.clone(),
                valid_hosts: hosts,
            }),
            None => Ok(endpoints
                [self.cursor.fetch_add(1, Ordering::Relaxed) as usize % endpoints.len()]
            .clone()),
        }
    }

    /// Reads and validates the inbound body.
    async fn read_body(&self, body: Body, headers: &HeaderMap) -> Result<Vec<u8>, GatewayError> {
        let encoding = headers
            .get(header::TRANSFER_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !encoding.is_empty() && !encoding.eq_ignore_ascii_case("chunked") {
            return Err(GatewayError::Validation(format!(
                "unsupported transfer encoding {encoding:?}; only chunked is supported"
            )));
        }
        let declared = content_length(headers)?;
        if let Some(len) = declared
            && len > self.config.max_body_bytes as u64 {
                return Err(GatewayError::PayloadTooLarge);
            }
        let bytes = axum::body::to_bytes(body, self.config.max_body_bytes)
            .await
            .map_err(|e| {
                if e.to_string().contains("length limit") {
                    GatewayError::PayloadTooLarge
                } else {
                    GatewayError::ProtocolError(format!("request body read failed: {e}"))
                }
            })?;
        if let Some(len) = declared
            && len != bytes.len() as u64 {
                return Err(GatewayError::Validation(format!(
                    "content-length {len} does not match the {}-byte body",
                    bytes.len()
                )));
            }
        Ok(bytes.to_vec())
    }

    /// Runs the auth plugins, in binding order.
    async fn authenticate(
        &self,
        bindings: &[crate::domain::plugin::PluginBinding],
        caller: &CallContext,
        upstream_id: Uuid,
    ) -> Result<crate::domain::plugin::AuthInjection, GatewayError> {
        let mut combined = crate::domain::plugin::AuthInjection::none();
        for binding in bindings {
            let Some(plugin) = self.auth.get(&binding.plugin_ref) else {
                // A custom plugin id cannot be executed here; built-ins are the
                // only injectors. Anything else is a binding the operator must
                // fix, reported as unresolvable.
                if !is_builtin_ref(&binding.plugin_ref) {
                    return Err(GatewayError::PluginNotFound(binding.plugin_ref.clone()));
                }
                continue;
            };
            let config = binding.config_map();
            let secrets = self.control_plane.secrets();
            let ctx = AuthContext {
                tenant_id: caller.tenant_id,
                subject_tenant_id: caller.tenant_id,
                subject_id: &caller.subject_id,
                upstream_id,
                config: &config,
                secrets: secrets.as_ref(),
            };
            let injection = plugin.authenticate(&ctx).await?;
            combined.headers.extend(injection.headers);
            combined.query.extend(injection.query);
        }
        Ok(combined)
    }

    /// Runs the request-side guard plugins, in binding order.
    fn guard_request(
        &self,
        bindings: &[crate::domain::plugin::PluginBinding],
        headers: &HeaderMap,
    ) -> Result<(), GatewayError> {
        for binding in bindings {
            let Some(plugin) = self.guards.get(&binding.plugin_ref) else {
                continue;
            };
            let config = binding.config_map();
            if let GuardVerdict::Reject(e) = plugin.guard_request(headers, &config) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Applies every configured rate limit and the upstream's strategy.
    async fn enforce_rate_limit(
        &self,
        upstream: &Upstream,
        route: Option<&crate::domain::model::Route>,
        caller: &CallContext,
        chain: &[Uuid],
    ) -> Result<(), GatewayError> {
        let mut candidates: Vec<RateLimitConfig> = Vec::new();
        // Ancestor-enforced limits apply to the descendant's traffic.
        for record in self.control_plane.store().upstreams_for(chain) {
            if record.spec.alias.eq_ignore_ascii_case(&upstream.alias)
                && let Some(limit) = record.spec.rate_limit.clone()
                    && limit.sharing == Sharing::Enforce {
                        candidates.push(limit);
                    }
        }
        if let Some(limit) = upstream.rate_limit.clone() {
            candidates.push(limit);
        }
        if let Some(limit) = route.and_then(|r| r.rate_limit.clone()) {
            candidates.push(limit);
        }
        let Some(limit) = effective_limit(&candidates) else {
            return Ok(());
        };

        let route_id = route.and_then(|r| r.id.as_deref()).map(|raw| route_gts_id(parse_resource_id(raw).unwrap_or_default()));
        let key = RateLimiter::scope_key(
            &limit,
            caller.tenant_id,
            &caller.subject_id,
            &caller.client_ip,
            route_id.as_deref().unwrap_or(""),
        );
        let cost = limit.cost.max(1);
        match self.limiter.check(&limit, &key, cost) {
            Ok(RateDecision::Allowed { .. }) => Ok(()),
            Err(e) => match limit.strategy {
                RateStrategy::Reject => Err(e),
                // `queue` waits for the bucket, bounded by the upstream timeout.
                RateStrategy::Queue => {
                    let wait =
                        e.retry_after_secs().unwrap_or(1).min(self.config.proxy_timeout_secs);
                    tokio::time::sleep(std::time::Duration::from_secs(wait.max(1))).await;
                    if self.limiter.check(&limit, &key, cost).is_ok() {
                        Ok(())
                    } else {
                        Err(e)
                    }
                }
                // `degrade` serves the request and marks the response.
                RateStrategy::Degrade => Ok(()),
            },
        }
    }

    /// Sends the request to `target` and relays the response.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        upstream: &Upstream,
        method: Method,
        target: &Endpoint,
        path: &str,
        query: &Option<String>,
        outbound_headers: HeaderMap,
        body: Vec<u8>,
        cors: HeaderMap,
        bindings: &[crate::domain::plugin::PluginBinding],
        upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<Response, GatewayError> {
        let uri = build_uri(target, path, query.as_deref())?;
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in outbound_headers.iter() {
            builder = builder.header(name, value);
        }
        // The request id was already set on `outbound_headers`; adding it here
        // as well would send the upstream two of them.
        let request = builder.body(Body::from(body)).map_err(|e| {
            GatewayError::ProtocolError(format!("outbound request could not be built: {e}"))
        })?;

        let response = tokio::time::timeout(self.config.proxy_timeout(), self.client.send(request))
            .await
            .map_err(|_| GatewayError::Timeout {
                kind: TimeoutKind::Request,
                detail: format!("upstream {} did not answer in time", upstream.alias),
            })??;

        relay_response(response, upstream, cors, bindings, &self.transforms, upgrade).await
    }
}

// ---------------------------------------------------------------- helpers

use axum::http::header;

impl ProxyRequest {
    /// The protocol the client asked to switch to, when `Connection: Upgrade`
    /// is also present.
    fn upgrade_protocol(&self) -> Option<String> {
        let upgrade = self
            .headers
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())?;
        self.headers
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.to_ascii_lowercase().contains("upgrade"))
            .map(|_| upgrade.to_owned())
    }
}

/// Carries the upgrade's negotiation headers over to the outbound request.
///
/// `Sec-WebSocket-*` are end-to-end handshake headers, not hop-by-hop ones, and
/// the upstream cannot complete the switch without them — but they are
/// `passthrough`-governed like any other header, so a route with the default
/// rules would drop them and leave the caller hanging on a `400`. An upgrade in
/// flight is the one case where they are the point of the request.
fn copy_upgrade_negotiation(inbound: &HeaderMap, outbound: &mut HeaderMap) {
    for (name, value) in inbound.iter() {
        if name.as_str().len() > 4 && name.as_str()[..4].eq_ignore_ascii_case("sec-") {
            outbound.entry(name.clone()).or_insert(value.clone());
        }
    }
}

/// The suffix as an upstream path: empty means the upstream root.
#[must_use]
pub fn normalize_suffix(raw: &str) -> String {
    if raw.is_empty() {
        "/".to_owned()
    } else if raw.starts_with('/') {
        raw.to_owned()
    } else {
        format!("/{raw}")
    }
}

/// Whether `method` is in the route's allowlist.
fn method_allowed_by(match_config: &HttpMatch, method: &Method) -> bool {
    match_config.methods.iter().any(|m| {
        let name = match m {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Patch => "PATCH",
        };
        name.eq_ignore_ascii_case(method.as_str())
    })
}

/// Whether `suffix` falls under the route's path prefix.
fn path_matches(match_config: &HttpMatch, suffix: &str) -> bool {
    let pattern = &match_config.path;
    if pattern == "/" || pattern.is_empty() {
        return true;
    }
    suffix == pattern || suffix.strip_prefix(pattern).is_some_and(|rest| rest.starts_with('/'))
}

/// Rejects a suffix beyond the route's path when the route forbids one.
fn validate_suffix(match_config: &HttpMatch, suffix: &str) -> Result<(), GatewayError> {
    if match_config.path_suffix_mode == SuffixMode::Disabled && suffix != match_config.path {
        return Err(GatewayError::Validation(format!(
            "this route does not accept a path suffix beyond {}",
            match_config.path
        )));
    }
    Ok(())
}

/// Rejects query parameters outside the route's allowlist. An empty allowlist
/// permits none at all.
fn validate_query(match_config: &HttpMatch, query: Option<&str>) -> Result<(), GatewayError> {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Ok(());
    };
    for (name, _) in form_urlencoded::parse(query.as_bytes()) {
        let name = name.into_owned();
        if !match_config
            .query_allowlist
            .iter()
            .any(|allowed| allowed == &name)
        {
            return Err(GatewayError::Validation(format!(
                "query parameter {name:?} is not allowed by this route"
            )));
        }
    }
    Ok(())
}

/// Rejects a method outside the route's allowlist.
fn validate_method(match_config: &HttpMatch, method: &Method) -> Result<(), GatewayError> {
    if method_allowed_by(match_config, method) {
        return Ok(());
    }
    Err(GatewayError::Validation(format!(
        "method {} is not accepted by this route",
        method.as_str()
    )))
}

/// The `X-OAGW-Target-Host` value, if the caller supplied one.
fn target_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(gts::TARGET_HOST_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
}

/// Whether the alias is the registrable suffix of a multi-host pool, which is
/// the shape that needs `X-OAGW-Target-Host` to disambiguate (ADR-0001).
fn is_common_suffix_pool(upstream: &Upstream) -> bool {
    let endpoints = &upstream.server.endpoints;
    endpoints.len() >= 2
        && compute_derived_alias(endpoints)
            .is_some_and(|derived| derived.eq_ignore_ascii_case(&upstream.alias))
}

/// Whether a plaintext upstream may be dialled under the current policy.
fn ensure_permitted_scheme(endpoint: &Endpoint, allow_http: bool) -> Result<(), GatewayError> {
    if matches!(endpoint.scheme, crate::domain::model::Scheme::Http) && !allow_http {
        return Err(GatewayError::InsecureUpstream(format!(
            "{}: plaintext upstream connections are disabled by policy",
            endpoint.host_port()
        )));
    }
    Ok(())
}

/// The `host` or `host:port` authority of an endpoint.
#[must_use]
pub fn authority(endpoint: &Endpoint) -> String {
    endpoint.host_port()
}

/// Builds an absolute URI for the upstream call.
fn build_uri(
    target: &Endpoint,
    path: &str,
    query: Option<&str>,
) -> Result<http::Uri, GatewayError> {
    let scheme = match target.scheme {
        crate::domain::model::Scheme::Http => HttpScheme::HTTP,
        _ => HttpScheme::HTTPS,
    };
    let authority: Authority = target.host_port().parse().map_err(|_| {
        GatewayError::ProtocolError(format!("invalid upstream authority {}", target.host_port()))
    })?;
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let pq = match query {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path,
    };
    http::Uri::builder()
        .scheme(scheme)
        .authority(authority)
        .path_and_query(pq)
        .build()
        .map_err(|e| GatewayError::ProtocolError(format!("invalid upstream URI: {e}")))
}

/// `Some(len)` when `Content-Length` is present and well-formed.
fn content_length(headers: &HeaderMap) -> Result<Option<u64>, GatewayError> {
    match headers.get(header::CONTENT_LENGTH) {
        None => Ok(None),
        Some(value) => {
            let text = value.to_str().map_err(|_| {
                GatewayError::Validation("content-length is not a valid integer".to_owned())
            })?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<u64>()
                .map(Some)
                .map_err(|_| GatewayError::Validation("content-length is not a valid integer".to_owned()))
        }
    }
}

/// The upstream path for this request.
fn upstream_path(match_config: &HttpMatch, suffix: &str) -> String {
    match match_config.path_suffix_mode {
        SuffixMode::Append => suffix.to_owned(),
        SuffixMode::Disabled => match_config.path.clone(),
    }
}

/// The query string for the upstream: the allowlisted inbound parameters, plus
/// anything an auth plugin appends.
fn outbound_query(
    inbound: Option<&str>,
    match_config: &HttpMatch,
    appended: &[(String, String)],
) -> Option<String> {
    let mut pairs: Vec<(String, String)> = form_urlencoded::parse(inbound.unwrap_or_default().as_bytes())
        .filter(|(name, _)| match_config.query_allowlist.iter().any(|a| a == &*name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    pairs.extend(appended.iter().cloned());
    if pairs.is_empty() {
        return None;
    }
    Some(
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish(),
    )
}

/// The bindings in execution order: upstream plugins, then route plugins.
fn collect_plugins(
    upstream: &Upstream,
    route: Option<&crate::domain::model::Route>,
) -> Vec<crate::domain::plugin::PluginBinding> {
    let mut out = Vec::new();
    if let Some(plugins) = upstream.plugins.as_ref() {
        out.extend(bindings_of(plugins));
    }
    if let Some(route) = route
        && let Some(plugins) = route.plugins.as_ref() {
            out.extend(bindings_of(plugins));
        }
    out
}

fn bindings_of(plugins: &PluginsConfig) -> Vec<crate::domain::plugin::PluginBinding> {
    plugins.items.iter().filter_map(binding_of).collect()
}

/// Reads one `plugins.items[]` entry: a bare identifier or an `ADR-0009`
/// binding object carrying `plugin_ref` and inline `config`.
fn binding_of(item: &serde_json::Value) -> Option<crate::domain::plugin::PluginBinding> {
    if let Some(name) = item.as_str() {
        return Some(crate::domain::plugin::PluginBinding {
            plugin_ref: name.to_owned(),
            config: None,
        });
    }
    let object = item.as_object()?;
    let plugin_ref = object.get("plugin_ref")?.as_str()?.to_owned();
    Some(crate::domain::plugin::PluginBinding {
        plugin_ref,
        config: object.get("config").cloned().filter(|c| !c.is_null()),
    })
}

/// Whether `plugin_ref` names one of the built-in plugins.
fn is_builtin_ref(plugin_ref: &str) -> bool {
    plugin_ref.starts_with("gts.cf.core.oagw.")
}

/// The inbound `X-Request-ID`, or a fresh one when the caller sent none.
fn request_id_header(headers: &HeaderMap) -> Option<HeaderValue> {
    headers
        .get(gts::REQUEST_ID_HEADER)
        .cloned()
        .or_else(|| HeaderValue::from_str(&format!("oagw-{}", Uuid::now_v7())).ok())
}

/// Relays the upstream response to the caller.
///
/// The body is handed over as it arrives: an unbounded stream is never
/// buffered, which is what keeps server-sent events flowing.
async fn relay_response(
    response: axum::http::Response<hyper::body::Incoming>,
    upstream: &Upstream,
    cors: HeaderMap,
    bindings: &[crate::domain::plugin::PluginBinding],
    transforms: &TransformPluginRegistry,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> Result<Response, GatewayError> {
    let status = response.status();
    let mut out = header_rules::outbound_response_headers(response.headers());
    if let Some(rules) = upstream.headers.as_ref().and_then(|h| h.response.as_ref()) {
        header_rules::apply_response_rules(&mut out, Some(rules));
    }
    for binding in bindings {
        if let Some(plugin) = transforms.get(&binding.plugin_ref) {
            plugin.transform_response(&mut out, &binding.config_map());
        }
    }
    for (name, value) in cors.iter() {
        out.insert(name, value.clone());
    }
    out.insert(
        HeaderName::from_static(gts::ERROR_SOURCE_HEADER),
        HeaderValue::from_static("upstream"),
    );

    if status == StatusCode::SWITCHING_PROTOCOLS {
        // The hop-by-hop strip would cost the caller the handshake itself: the
        // `101` is meaningless without the two headers that name the switch.
        for name in [header::UPGRADE, header::CONNECTION] {
            if let Some(value) = response.headers().get(&name) {
                out.insert(name, value.clone());
            }
        }
        return relay_upgrade(response, out, upgrade).await;
    }

    let body = response.into_body();
    let mut builder = Response::builder().status(status);
    for (name, value) in out.iter() {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::new(body))
        .map_err(|e| GatewayError::Internal(format!("relay failed: {e}")))
}

/// Splices the caller's upgraded socket to the upstream's.
///
/// `upgrade` is the pending upgrade of the *caller's* connection; it resolves
/// once the `101` has been sent and hyper has taken the socket over.
async fn relay_upgrade(
    response: axum::http::Response<hyper::body::Incoming>,
    headers: HeaderMap,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> Result<Response, GatewayError> {
    let Some(caller) = upgrade else {
        return Err(GatewayError::StreamAborted(
            "the upstream switched protocols but the client did not ask to".to_owned(),
        ));
    };
    let mut upstream_response = response;
    let upstream_io = hyper::upgrade::on(&mut upstream_response)
        .await
        .map_err(|e| GatewayError::StreamAborted(format!("upstream upgrade failed: {e}")))?;

    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    let client_response = builder
        .body(Body::empty())
        .map_err(|e| GatewayError::Internal(format!("relay failed: {e}")))?;

    tokio::spawn(async move {
        // `caller` resolves once the `101` has been written and hyper has taken
        // the socket over; both ends then speak hyper's Read/Write, so wrap them
        // for tokio's copy.
        let mut caller = match caller.await {
            Ok(io) => hyper_util::rt::TokioIo::new(io),
            Err(e) => {
                tracing::debug!(error = %e, "caller upgrade never completed");
                return;
            }
        };
        let mut upstream = hyper_util::rt::TokioIo::new(upstream_io);
        match tokio::io::copy_bidirectional(&mut caller, &mut upstream).await {
            Ok((a, b)) => tracing::debug!(client_to_upstream = a, upstream_to_client = b, "upgraded connection closed"),
            Err(e) => tracing::debug!(error = %e, "upgraded connection closed with an error"),
        }
    });
    Ok(client_response)
}
