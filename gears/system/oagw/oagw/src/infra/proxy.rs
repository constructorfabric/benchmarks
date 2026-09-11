//! The data plane: request classification, endpoint selection, plugin
//! execution, rate limiting and forwarding.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::response::IntoResponse as _;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use dashmap::DashMap;
use toolkit_security::SecurityContext;

use crate::api::error::{ERROR_SOURCE_NAME, OagwError};
use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::ids;
use crate::domain::model::{
    Endpoint, MapCodec, Passthrough, Route, Upstream, VecMapCodec,
};
use crate::domain::model::PluginType;
use crate::domain::plugin::{GuardDecision, PluginError, PluginRegistries, RequestContext};
use crate::domain::ratelimit::{self, TokenBucket};
use crate::domain::service::{ControlPlaneService, MatchedRoute, ResolvedUpstream};
use crate::infra::control_plane::is_common_suffix_alias;

/// Routing header read for endpoint selection and then stripped.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Headers that never leave the gateway.
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers consumed by the gateway and never forwarded as they arrived.
const ROUTING_HEADERS: &[&str] = &["host", TARGET_HOST_HEADER, "content-length"];

/// Everything the proxy needs about one inbound request.
#[derive(Debug, Clone)]
pub struct ProxiedRequest {
    /// Addressed alias.
    pub alias: String,
    /// Request method.
    pub method: Method,
    /// Path after the alias, always starting with `/`.
    pub path_suffix: String,
    /// Query parameters in request order.
    pub query: Vec<(String, String)>,
    /// Inbound headers, lowercase names.
    pub headers: Vec<(String, String)>,
    /// Buffered body.
    pub body: bytes::Bytes,
    /// Client address, when the host runtime reported one.
    pub client_ip: String,
    /// Whether the caller asked for a `WebSocket` upgrade.
    pub is_websocket: bool,
}

impl ProxiedRequest {
    /// Reads the first value of an inbound header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let lowered = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == lowered)
            .map(|(_, value)| value.as_str())
    }

    /// True when the request carries a non-empty `Origin` header.
    #[must_use]
    pub fn is_cross_origin(&self) -> bool {
        self.header("origin").is_some_and(|value| !value.trim().is_empty())
    }
}

/// A rate-limit counter together with the configuration it was built from.
struct Bucket {
    fingerprint: String,
    bucket: TokenBucket,
}

type HyperConnector =
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;

/// The proxy engine.
pub struct ProxyEngine {
    control_plane: Arc<dyn ControlPlaneService>,
    registries: Arc<PluginRegistries>,
    config: OagwConfig,
    client: hyper_util::client::legacy::Client<HyperConnector, Body>,
    round_robin: AtomicUsize,
    buckets: DashMap<String, Bucket>,
}

/// A chosen endpoint together with the admitted target hosts.
#[derive(Debug, Clone)]
pub struct SelectedEndpoint {
    /// The endpoint.
    pub endpoint: Endpoint,
    /// Values admitted by `X-OAGW-Target-Host`.
    pub valid_hosts: Vec<String>,
}

impl ProxyEngine {
    /// Builds the engine.
    ///
    /// # Panics
    ///
    /// Panics when the platform certificate store cannot be loaded, which
    /// would leave the gateway unable to reach any `TLS` upstream.
    #[must_use]
    pub fn new(
        control_plane: Arc<dyn ControlPlaneService>,
        registries: Arc<PluginRegistries>,
        config: OagwConfig,
    ) -> Self {
        let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
        connector.set_connect_timeout(Some(config.connect_timeout()));
        connector.set_nodelay(true);
        // The platform trust store is a startup invariant: without it the
        // gateway cannot reach any TLS upstream, so failing fast here is the
        // documented behaviour (see the `# Panics` section).
        #[allow(clippy::expect_used)]
        let tls = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("platform certificates are loadable")
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(connector);
        let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build::<HyperConnector, Body>(tls);
        Self {
            control_plane,
            registries,
            config,
            client,
            round_robin: AtomicUsize::new(0),
            buckets: DashMap::new(),
        }
    }

    /// The plugin registries.
    #[must_use]
    pub fn registries(&self) -> Arc<PluginRegistries> {
        Arc::clone(&self.registries)
    }

    /// Runs one proxied request; every failure is rendered as a response.
    pub async fn handle(
        &self,
        context: &SecurityContext,
        request: ProxiedRequest,
        upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> axum::response::Response {
        let started = std::time::Instant::now();
        let outcome = self.execute(context, &request, upgrade).await;
        let response = match outcome {
            Ok(response) => response,
            Err(error) => error
                .with_instance(format!("/oagw/v1/proxy/{}{}", request.alias, request.path_suffix))
                .into_response(),
        };
        let status = response.status().as_u16();
        tracing::info!(
            target: "oagw::proxy",
            correlation_id = request.header("x-request-id").unwrap_or_default(),
            alias = %request.alias,
            path = %request.path_suffix,
            method = %request.method,
            status,
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "proxied request"
        );
        response
    }

    async fn execute(
        &self,
        context: &SecurityContext,
        request: &ProxiedRequest,
        upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<axum::response::Response, OagwError> {
        let Some(resolved) = self
            .control_plane
            .resolve_alias(context.subject_tenant_id(), &request.alias)
            .await
        else {
            return Err(OagwError::route_not_found(format!(
                "no upstream is registered under alias '{}'",
                request.alias
            )));
        };
        if !resolved.upstream.enabled {
            return Err(OagwError::link_unavailable(format!(
                "upstream '{}' is disabled",
                request.alias
            )));
        }

        let endpoint = self.select_endpoint(&resolved, request)?;
        let matched = self
            .control_plane
            .match_route(&resolved, request.method.as_str(), &request.path_suffix)
            .await
            .ok_or_else(|| {
                OagwError::route_not_found(format!(
                    "no enabled route matches {} {} for alias '{}'",
                    request.method, request.path_suffix, request.alias
                ))
            })?;

        Self::apply_cors(&resolved.upstream, request)?;
        Self::reject_unwanted_suffix(&matched, request)?;
        Self::check_query_allowlist(&matched, request)?;
        Self::validate_body(request)?;

        let effective_limit = self
            .control_plane
            .effective_rate_limit(context.subject_tenant_id(), &request.alias, Some(&matched.route))
            .await;
        if let Some(limit) = &effective_limit {
            self.enforce_rate_limit(limit, context, &matched, request)?;
        }

        let mut plugin_ctx = Self::build_plugin_context(context, request);
        self.run_request_plugins(&resolved.upstream, &matched.route, &mut plugin_ctx)
            .await
            .map_err(|error| plugin_error(&error))?;

        self.forward(
            &resolved.upstream,
            &endpoint.endpoint,
            request,
            &matched,
            &plugin_ctx,
            upgrade,
        )
        .await
    }

    // -- routing ---------------------------------------------------------

    fn select_endpoint(
        &self,
        resolved: &ResolvedUpstream,
        request: &ProxiedRequest,
    ) -> Result<SelectedEndpoint, OagwError> {
        let endpoints = resolved.upstream.server.endpoints.clone();
        let valid_hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();
        let common_suffix = is_common_suffix_alias(&resolved.upstream);

        let Some(requested) = request
            .header(TARGET_HOST_HEADER)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            if endpoints.len() == 1 {
                return Ok(SelectedEndpoint { endpoint: endpoints[0].clone(), valid_hosts });
            }
            if common_suffix {
                return Err(OagwError::routing(
                    ids::ERR_MISSING_TARGET_HOST,
                    "Target Host Required",
                    "X-OAGW-Target-Host is required for a common-suffix alias",
                )
                .with_extension("valid_hosts", serde_json::json!(valid_hosts)));
            }
            let index = self.round_robin.fetch_add(1, Ordering::Relaxed) % endpoints.len();
            return Ok(SelectedEndpoint {
                endpoint: endpoints[index].clone(),
                valid_hosts,
            });
        };

        alias::validate_target_host(requested).map_err(|reason| {
            OagwError::routing(
                ids::ERR_INVALID_TARGET_HOST,
                "Invalid Target Host",
                format!("X-OAGW-Target-Host '{requested}' is not a hostname or IP: {reason}"),
            )
        })?;
        let endpoint = endpoints
            .iter()
            .find(|candidate| candidate.host == requested)
            .ok_or_else(|| {
                OagwError::routing(
                    ids::ERR_UNKNOWN_TARGET_HOST,
                    "Unknown Target Host",
                    format!("X-OAGW-Target-Host '{requested}' does not name a configured endpoint"),
                )
                .with_extension("valid_hosts", serde_json::json!(valid_hosts))
            })?;
        Ok(SelectedEndpoint {
            endpoint: endpoint.clone(),
            valid_hosts,
        })
    }

    fn apply_cors(upstream: &Upstream, request: &ProxiedRequest) -> Result<(), OagwError> {
        if !request.is_cross_origin() {
            return Ok(());
        }
        let Some(origin) = request.header("origin") else {
            return Ok(());
        };
        let Some(cors) = upstream.cors.as_ref().filter(|cors| cors.enabled) else {
            return Err(OagwError::cors_origin_not_allowed(format!(
                "origin '{origin}' is not allowed: CORS is not enabled for this upstream"
            )));
        };
        if !cors.allows_origin(origin) {
            return Err(OagwError::cors_origin_not_allowed(format!(
                "origin '{origin}' is not allowed for this upstream"
            )));
        }
        if !cors.allows_method(request.method.as_str()) {
            return Err(OagwError::cors_method_not_allowed(format!(
                "method {} is not allowed cross-origin for this upstream",
                request.method
            )));
        }
        Ok(())
    }

    fn reject_unwanted_suffix(matched: &MatchedRoute, request: &ProxiedRequest) -> Result<(), OagwError> {
        let Some(http) = &matched.route.match_rules.http else {
            return Ok(());
        };
        if http.path_suffix_mode == crate::domain::model::PathSuffixMode::Disabled
            && match_suffix_remainder(http.path.as_str(), &request.path_suffix)
                .is_some_and(|rest| !rest.is_empty())
        {
            return Err(OagwError::validation(format!(
                "route path '{}' does not accept a path suffix",
                http.path
            )));
        }
        Ok(())
    }

    fn check_query_allowlist(matched: &MatchedRoute, request: &ProxiedRequest) -> Result<(), OagwError> {
        let Some(http) = &matched.route.match_rules.http else {
            return Ok(());
        };
        for (name, _) in &request.query {
            if !http.query_allowlist.iter().any(|allowed| allowed == name) {
                return Err(OagwError::validation(format!(
                    "query parameter '{name}' is not in the route's query_allowlist"
                )));
            }
        }
        Ok(())
    }

    fn validate_body(request: &ProxiedRequest) -> Result<(), OagwError> {
        if let Some(encoding) = request.header("transfer-encoding") {
            let unsupported = encoding
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .any(|value| !value.eq_ignore_ascii_case("chunked"));
            if unsupported {
                return Err(OagwError::validation(format!(
                    "unsupported transfer encoding '{encoding}'"
                )));
            }
        }
        if let Some(declared) = request.header("content-length") {
            let Ok(expected) = declared.trim().parse::<usize>() else {
                return Err(OagwError::validation(format!(
                    "content-length '{declared}' is not a valid integer"
                )));
            };
            if expected != request.body.len() {
                return Err(OagwError::validation(format!(
                    "content-length {expected} does not match the actual body size {}",
                    request.body.len()
                )));
            }
        }
        if request.body.len() > crate::config::MAX_BODY_BYTES {
            return Err(OagwError::payload_too_large(format!(
                "request body exceeds the {} byte limit",
                crate::config::MAX_BODY_BYTES
            )));
        }
        Ok(())
    }

    fn enforce_rate_limit(
        &self,
        limit: &crate::domain::model::RateLimit,
        context: &SecurityContext,
        matched: &MatchedRoute,
        request: &ProxiedRequest,
    ) -> Result<(), OagwError> {
        let tenant_id = context.subject_tenant_id().to_string();
        let subject_id = context.subject_id().to_string();
        let parts = ratelimit::ScopeParts {
            tenant_id: &tenant_id,
            subject_id: &subject_id,
            client_ip: &request.client_ip,
            upstream_id: matched.route.upstream_id.as_str(),
            route_id: matched.route.id.as_deref().unwrap_or_default(),
        };
        let key = ratelimit::counter_key(limit.scope, parts);
        let fingerprint = format!("{limit:?}");
        let now = std::time::Instant::now();
        let mut entry = self.buckets.entry(key).or_insert_with(|| Bucket {
            fingerprint: fingerprint.clone(),
            bucket: TokenBucket::from_config(limit, now),
        });
        if entry.fingerprint != fingerprint {
            entry.fingerprint.clone_from(&fingerprint);
            entry.bucket = TokenBucket::from_config(limit, now);
        }
        let outcome = entry.bucket.try_take(limit.cost, now);
        if !outcome.allowed {
            let reset = outcome
                .reset_at
                .checked_duration_since(now)
                .map_or(1, |left| left.as_secs().max(1));
            return Err(OagwError::rate_limit_exceeded(format!(
                "rate limit of {} requests per {:?} exceeded",
                limit.sustained.rate, limit.sustained.window
            ))
            .with_retry_after(reset)
            .with_rate_limit_headers(limit.sustained.rate, 0, reset));
        }
        Ok(())
    }

    // -- plugins ---------------------------------------------------------

    fn build_plugin_context(context: &SecurityContext, request: &ProxiedRequest) -> RequestContext {
        RequestContext {
            headers: request.headers.clone(),
            query: request.query.clone(),
            subject_id: context.subject_id(),
            subject_tenant_id: context.subject_tenant_id(),
            ..RequestContext::default()
        }
    }

    /// Auth first, then guards, then transforms, upstream bindings first.
    async fn run_request_plugins(
        &self,
        upstream: &Upstream,
        route: &Route,
        plugin_ctx: &mut RequestContext,
    ) -> Result<(), PluginError> {
        if let Some(auth) = &upstream.auth
            && let Some(plugin) = self.registries.auth.resolve(&auth.auth_type)
        {
            plugin.authenticate(plugin_ctx, &auth.config).await?;
            plugin_ctx.record("auth");
        }

        let bindings = self.ordered_bindings(upstream, route);
        for (reference, config, kind) in bindings {
            match kind {
                PluginType::Guard => {
                    let Some(plugin) = self.registries.guard.resolve(&reference) else {
                        continue;
                    };
                    match plugin.guard_request(plugin_ctx, &config).await {
                        Ok(GuardDecision::Allow) => {}
                        Ok(GuardDecision::Reject(error)) | Err(error) => return Err(error),
                    }
                    plugin_ctx.record("guard");
                }
                PluginType::Transform => {
                    if let Some(plugin) = self.registries.transform.resolve(&reference) {
                        plugin.transform_request(plugin_ctx, &config).await?;
                        plugin_ctx.record("transform");
                    }
                }
                PluginType::Auth => {}
            }
        }
        Ok(())
    }

    /// Guards then transforms on the upstream response.
    async fn run_response_plugins(
        &self,
        upstream: &Upstream,
        route: &Route,
        plugin_ctx: &mut RequestContext,
    ) -> Result<(), PluginError> {
        for (reference, config, kind) in self.ordered_bindings(upstream, route) {
            match kind {
                PluginType::Guard => {
                    if let Some(plugin) = self.registries.guard.resolve(&reference) {
                        match plugin.guard_response(plugin_ctx, &config).await {
                            Ok(GuardDecision::Allow) => {}
                            Ok(GuardDecision::Reject(error)) | Err(error) => return Err(error),
                        }
                    }
                }
                PluginType::Transform => {
                    if let Some(plugin) = self.registries.transform.resolve(&reference) {
                        plugin.transform_response(plugin_ctx, &config).await?;
                    }
                }
                PluginType::Auth => {}
            }
        }
        Ok(())
    }

    // -- forwarding ------------------------------------------------------

    #[allow(clippy::too_many_lines)]
    async fn forward(
        &self,
        upstream: &Upstream,
        endpoint: &Endpoint,
        request: &ProxiedRequest,
        matched: &MatchedRoute,
        plugin_ctx: &RequestContext,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<axum::response::Response, OagwError> {
        let Some(route_path) = crate::domain::validation::route_path(&matched.route.match_rules)
        else {
            return Err(OagwError::route_not_found(
                "the matched route is not an HTTP route",
            ));
        };
        let suffix = match_suffix_remainder(route_path, &request.path_suffix).unwrap_or_default();
        let url = outbound_url(endpoint, &outbound_path(route_path, &suffix), &outbound_query(request, plugin_ctx));

        let mut builder = http::Request::builder()
            .method(request.method.clone())
            .uri(url.clone());
        if let Some(host) = outbound_host(endpoint) {
            builder = builder.header("host", host);
        }
        for (name, value) in outbound_headers(upstream, request, plugin_ctx) {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if request.is_websocket {
            builder = builder
                .header("connection", "upgrade")
                .header("upgrade", "websocket");
            // The handshake headers are part of the upgrade, not of the
            // configurable passthrough, so they are replayed verbatim.
            for (name, value) in &request.headers {
                if name.starts_with("sec-websocket-") {
                    builder = builder.header(name.as_str(), value.as_str());
                }
            }
        }
        let body = if request.body.is_empty() {
            Body::empty()
        } else {
            Body::from(request.body.clone())
        };
        let outbound = builder
            .body(body)
            .map_err(|error| OagwError::internal(format!("failed to build the upstream request: {error}")))?;

        let call = self.client.request(outbound);
        let response = match tokio::time::timeout(self.config.proxy_timeout(), call).await {
            Err(_) => {
                return Err(OagwError::timeout(format!(
                    "upstream '{}' did not answer within {:?}",
                    request.alias,
                    self.config.proxy_timeout()
                )));
            }
            Ok(Err(error)) => {
                tracing::debug!(error = %error, "upstream call failed");
                // A connector-level connect timeout is the documented
                // `ConnectionTimeout` (504, retriable); any other transport
                // failure is `DownstreamError` (502).
                if is_connect_timeout(&error) {
                    return Err(OagwError::connect_timeout(format!(
                        "upstream '{}' did not accept a connection within {:?}",
                        request.alias,
                        self.config.connect_timeout()
                    )));
                }
                return Err(OagwError::downstream(format!(
                    "upstream '{}' could not be reached: {error}",
                    request.alias
                )));
            }
            Ok(Ok(response)) => response,
        };

        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            return replay_upgrade(response, client_upgrade);
        }
        if request.is_websocket {
            return Err(OagwError::downstream(format!(
                "upstream '{}' did not accept the WebSocket upgrade",
                request.alias
            )));
        }

        let (parts, body) = response.into_parts();
        let mut plugin_ctx = plugin_ctx.clone();
        plugin_ctx.response_headers = header_pairs(&parts.headers);
        if let Err(error) = self
            .run_response_plugins(upstream, &matched.route, &mut plugin_ctx)
            .await
        {
            return Err(plugin_error(&error));
        }

        let mut headers = response_headers(upstream, &parts.headers, &plugin_ctx);
        if parts.status.is_client_error() || parts.status.is_server_error() {
            headers.insert(
                ERROR_SOURCE_NAME.clone(),
                HeaderValue::from_static(crate::api::error::SOURCE_UPSTREAM),
            );
        }
        apply_cors_response_headers(&mut headers, upstream, request);

        let streamed = Body::new(http_body_util::BodyStream::new(body));
        let mut response = axum::response::Response::new(streamed);
        *response.status_mut() = parts.status;
        *response.headers_mut() = headers;
        Ok(response)
    }

    /// Upstream-then-route bindings, resolved against the registries.
    ///
    /// Bindings that resolve to no implementation are dropped so an
    /// unresolvable reference cannot abort an in-flight request.
    #[must_use]
    fn ordered_bindings(
        &self,
        upstream: &Upstream,
        route: &Route,
    ) -> Vec<(String, serde_json::Value, PluginType)> {
        upstream
            .plugins
            .items
            .iter()
            .chain(route.plugins.items.iter())
            .filter_map(|binding| {
                let kind = self.registries.resolves(binding.plugin_ref())?;
                Some((
                    binding.plugin_ref().to_owned(),
                    binding.config().clone(),
                    kind,
                ))
            })
            .collect()
    }
}

/// Lowercases header names and lossily decodes their values.
#[must_use]
pub fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Applies the upstream's response header rules to the client response.
#[must_use]
pub fn response_headers(
    upstream: &Upstream,
    upstream_headers: &HeaderMap,
    plugin_ctx: &RequestContext,
) -> HeaderMap {
    let rules = upstream.headers.as_ref().and_then(|headers| headers.response.as_ref());
    let (set, add, remove) = rules.map_or_else(
        || (MapCodec::default(), VecMapCodec::default(), Vec::new()),
        |rules| (rules.set.clone(), rules.add.clone(), rules.remove.clone()),
    );

    let mut pairs: Vec<(String, String)> = header_pairs(upstream_headers);
    for (name, value) in &plugin_ctx.response_headers {
        if !pairs.iter().any(|(key, existing)| key == name && existing == value) {
            pairs.push((name.clone(), value.clone()));
        }
    }
    for name in &remove {
        pairs.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    }
    for (name, value) in &set {
        pairs.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        pairs.push((name.to_ascii_lowercase(), value.clone()));
    }
    for (name, values) in &add {
        for value in values {
            pairs.push((name.to_ascii_lowercase(), value.clone()));
        }
    }

    let mut headers = HeaderMap::with_capacity(pairs.len());
    for (name, value) in pairs {
        if is_hop_by_hop(&name) || name == "content-length" {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(&value),
        ) {
            headers.append(name, value);
        }
    }
    headers
}

/// Adds the `CORS` response headers for an allowed cross-origin request.
pub fn apply_cors_response_headers(
    headers: &mut HeaderMap,
    upstream: &Upstream,
    request: &ProxiedRequest,
) {
    if !request.is_cross_origin() {
        return;
    }
    let Some(cors) = upstream.cors.as_ref().filter(|cors| cors.enabled) else {
        return;
    };
    let Some(origin) = request.header("origin") else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(HeaderName::from_static("access-control-allow-origin"), value);
    }
    append_once(headers, "vary", "Origin");
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert(
            HeaderName::from_static("access-control-expose-headers"),
            value,
        );
    }
    if cors.allow_credentials {
        headers.insert(
            HeaderName::from_static("access-control-allow-credentials"),
            HeaderValue::from_static("true"),
        );
    }
}

/// Adds `Vary` without duplicating an existing value.
fn append_once(headers: &mut HeaderMap, name: &'static str, value: &str) {
    let key = HeaderName::from_static(name);
    let present = headers
        .get(&key)
        .map(|existing| existing.to_str().unwrap_or_default())
        .is_some_and(|existing| existing.split(',').any(|part| part.trim() == value));
    if present {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.append(key, value);
    }
}

/// Builds the outbound header list.
///
/// `passthrough` governs the inbound headers; anything a plugin wrote is
/// always forwarded, and `remove`/`set`/`add` are applied last.
#[must_use]
pub fn outbound_headers(
    upstream: &Upstream,
    request: &ProxiedRequest,
    plugin_ctx: &RequestContext,
) -> Vec<(String, String)> {
    let rules = upstream.headers.as_ref().and_then(|headers| headers.request.as_ref());
    let (passthrough, allowlist, set, add, remove) = rules.map_or_else(
        || (Passthrough::None, Vec::new(), MapCodec::default(), MapCodec::default(), Vec::new()),
        |rules| (
            rules.passthrough,
            rules.passthrough_allowlist.clone(),
            rules.set.clone(),
            rules.add.clone(),
            rules.remove.clone(),
        ),
    );

    let mut outbound: Vec<(String, String)> = Vec::new();
    for (name, value) in &request.headers {
        if is_hop_by_hop(name) || is_routing_header(name) {
            continue;
        }
        let forwarded = match passthrough {
            Passthrough::None => false,
            Passthrough::All => true,
            Passthrough::Allowlist => {
                allowlist.iter().any(|allowed| allowed.eq_ignore_ascii_case(name))
            }
        };
        if forwarded && !plugin_ctx.touched.contains(name) {
            outbound.push((name.clone(), value.clone()));
        }
    }
    for (name, value) in &plugin_ctx.headers {
        if plugin_ctx.touched.contains(name) {
            outbound.push((name.clone(), value.clone()));
        }
    }
    for name in &remove {
        outbound.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    }
    for (name, value) in &set {
        outbound.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        outbound.push((name.to_ascii_lowercase(), value.clone()));
    }
    for (name, value) in &add {
        outbound.push((name.to_ascii_lowercase(), value.clone()));
    }
    if let Some(credential) = &plugin_ctx.credential
        && let crate::domain::plugin::Credential::Header(name, value) = credential
    {
        outbound.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        outbound.push((name.clone(), value.clone()));
    }
    outbound
}

/// Builds the outbound query string from the allowlisted parameters plus any
/// credential the auth plugin injected into the query.
#[must_use]
pub fn outbound_query(request: &ProxiedRequest, plugin_ctx: &RequestContext) -> String {
    let mut pairs: Vec<(String, String)> = request.query.clone();
    if let Some(credential) = &plugin_ctx.credential
        && let crate::domain::plugin::Credential::Query(name, value) = credential
    {
        pairs.push((name.clone(), value.clone()));
    }
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(&name, &value);
    }
    serializer.finish()
}

/// The path forwarded upstream: the route path with the remainder appended.
#[must_use]
pub fn outbound_path(route_path: &str, suffix: &str) -> String {
    let route = route_path.trim_end_matches('/');
    format!("{route}{suffix}")
}

/// The `Host` header value for an endpoint, `None` when the default applies.
#[must_use]
pub fn outbound_host(endpoint: &Endpoint) -> Option<String> {
    if endpoint.port == endpoint.scheme.standard_port() {
        return None;
    }
    Some(format!("{}:{}", endpoint.host, endpoint.port))
}

/// The dialled `URL`.
#[must_use]
pub fn outbound_url(endpoint: &Endpoint, path: &str, query: &str) -> String {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if query.is_empty() {
        format!(
            "{}://{}:{}{}",
            endpoint.scheme.url_scheme(),
            endpoint.host,
            endpoint.port,
            path
        )
    } else {
        format!(
            "{}://{}:{}{}?{}",
            endpoint.scheme.url_scheme(),
            endpoint.host,
            endpoint.port,
            path,
            query
        )
    }
}

/// True when a header must never be forwarded.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&lowered.as_str())
}

/// True when an inbound header is consumed by the gateway.
#[must_use]
pub fn is_routing_header(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    ROUTING_HEADERS.contains(&lowered.as_str())
}

/// The remainder of `request_suffix` after `route_path`, when it matches.
#[must_use]
pub fn match_suffix_remainder(route_path: &str, request_suffix: &str) -> Option<String> {
    let route = route_path.trim_end_matches('/');
    let request = if request_suffix.is_empty() { "/" } else { request_suffix };
    if request == route {
        return Some(String::new());
    }
    if route != "/" && request.starts_with(&format!("{route}/")) {
        return Some(request[route.len()..].to_owned());
    }
    None
}

/// Replays an upstream `101` to the caller and bridges the two streams.
fn replay_upgrade(
    upstream_response: http::Response<hyper::body::Incoming>,
    client_upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> Result<axum::response::Response, OagwError> {
    let Some(client_upgrade) = client_upgrade else {
        return Err(OagwError::downstream(
            "the upstream answered with a protocol switch the client did not request",
        ));
    };
    let (mut parts, _) = upstream_response.into_parts();
    let upstream_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    let upgrade_token = parts.headers.get("upgrade").cloned();

    let mut response = axum::response::Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
            response.headers_mut().insert(name.clone(), value);
        }
    }
    // A `101` must advertise the switch. `Connection` is hop-by-hop and
    // `Upgrade` travels with it, so both are restored here.
    response.headers_mut().insert(
        http::header::CONNECTION,
        HeaderValue::from_static("upgrade"),
    );
    response
        .headers_mut()
        .entry(http::header::UPGRADE)
        .or_insert_with(|| {
            upgrade_token.unwrap_or_else(|| HeaderValue::from_static("websocket"))
        });
    response.extensions_mut().insert(client_upgrade.clone());

    if let Some(upstream_upgrade) = upstream_upgrade {
        tokio::spawn(async move {
            let mut upstream = match upstream_upgrade.await {
                Ok(upgraded) => hyper_util::rt::TokioIo::new(upgraded),
                Err(error) => {
                    tracing::debug!(error = %error, "upstream upgrade failed");
                    return;
                }
            };
            let mut client = match client_upgrade.await {
                Ok(upgraded) => hyper_util::rt::TokioIo::new(upgraded),
                Err(_) => return,
            };
            let copied = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            if let Err(error) = copied {
                tracing::debug!(error = %error, "websocket relay ended with an error");
            }
        });
    }
    Ok(response)
}

/// True when a transport error is the connector giving up on establishing a
/// connection rather than a failure of the connection itself.
///
/// `hyper_util`'s legacy error only advertises `is_connect()`; the timeout is
/// buried in the cause chain as an `io::Error` of `ErrorKind::TimedOut`, so it
/// is walked here.
#[must_use]
pub fn is_connect_timeout(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    for _ in 0..6 {
        let Some(current) = cause else { break };
        if let Some(io) = current.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::TimedOut
        {
            return true;
        }
        cause = current.source();
    }
    false
}

/// Maps a plugin failure onto the documented gateway error.
#[must_use]
pub fn plugin_error(error: &PluginError) -> OagwError {
    let status = StatusCode::from_u16(error.status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match &error {
        PluginError::SecretNotFound(name) => OagwError::secret_not_found(format!(
            "secret '{name}' could not be resolved"
        )),
        PluginError::AuthFailed(detail) => OagwError::auth_failed(detail.clone()),
        PluginError::GuardRejected { detail, .. } => {
            OagwError::new(status, &error.error_type(), "Request Rejected", detail.clone())
                .with_code(error.code())
        }
        PluginError::NotImplemented => {
            OagwError::plugin_not_found("the bound plugin has no implementation")
        }
    }
}
