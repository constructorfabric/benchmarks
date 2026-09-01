//! Data-plane proxy engine (DESIGN §3.2 "Proxy API", §3.5 proxy flow).
//!
//! The engine turns a resolved request plus the inbound HTTP exchange into an
//! outbound hop: it sanitises headers, runs the plugin chain, dials the
//! upstream and projects the upstream response back onto the caller. Streaming
//! (SSE) and WebSocket tunnels are handled here so the REST handler stays thin.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::api::error::{ERROR_SOURCE_UPSTREAM, upstream_error};
use crate::config::OagwConfig;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{Endpoint, Passthrough, RequestHeaders, ResponseHeaders, Scheme};
use crate::domain::plugin::{
    GuardDecision, GuardPluginRegistry, RequestContext, ResponseContext, TransformPluginRegistry,
    UnknownPlugin,
};
use crate::domain::services::proxy::ResolvedRequest;
use crate::infra::audit;
use crate::infra::cors::{self, SimpleCors};
use crate::infra::http::{
    OutboundClient, OutboundRequest, connection_named, host_token, insert_header, is_hop_by_hop,
    is_websocket_upgrade,
};
use crate::infra::metrics::SharedMetrics;
use crate::infra::plugin::{
    CredentialResolver, RequestIdTransformPlugin, auth_registry_with_builtins,
};
use crate::infra::ssrf;

/// Header carrying the caller's pinned endpoint host.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";
/// Header carrying the resolved upstream's GTS identifier.
pub const UPSTREAM_ID_HEADER: &str = "x-oagw-upstream-id";
/// Header carrying the matched route's GTS identifier.
pub const ROUTE_ID_HEADER: &str = "x-oagw-route-id";
/// Header carrying the error-source attribution (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Headers the gateway owns: a caller may not inject them, and an upstream's
/// copy is dropped before the response is re-stamped.
pub const GATEWAY_CONTROL_HEADERS: [&str; 5] = [
    TARGET_HOST_HEADER,
    "x-oagw-client-ip",
    ERROR_SOURCE_HEADER,
    UPSTREAM_ID_HEADER,
    ROUTE_ID_HEADER,
];

/// `true` when the upstream content type is a server-sent-event stream.
#[must_use]
pub fn is_event_stream(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        .unwrap_or(false)
}

/// Builds the outbound URL for an endpoint and a request path.
#[must_use]
pub fn outbound_url(endpoint: &Endpoint, path: &str) -> String {
    let scheme = endpoint.scheme.as_str();
    let host = host_token(&endpoint.host);
    let path = if path.is_empty() {
        "/"
    } else if path.starts_with('/') {
        path
    } else {
        return format!(
            "{scheme}://{host}:{port}/{path}",
            host = host,
            port = endpoint.port
        );
    };
    format!(
        "{scheme}://{host}:{port}{path}",
        host = host,
        port = endpoint.port
    )
}

/// Joins the admitted query string onto the outbound path.
///
/// A route's allowlist has already filtered `resolved.query`, so whatever
/// survives is what the upstream receives verbatim.
#[must_use]
pub fn with_query(path: &str, query: Option<&str>) -> String {
    match query {
        Some(query) if !query.is_empty() => format!("{path}?{query}"),
        _ => path.to_owned(),
    }
}

/// `true` when the path stays inside its root: no `.`/`..` segment and no
/// percent-encoded spelling of one survives decoding.
#[must_use]
pub fn is_canonical_path(path: &str) -> bool {
    path.split('/').all(|segment| {
        if matches!(segment, "." | "..") {
            return false;
        }
        let decoded = crate::domain::services::proxy::percent_decode(segment);
        !decoded.is_some_and(|decoded| matches!(decoded.as_str(), "." | ".."))
    })
}

/// The upstream-facing request path: the matched route prefix plus the suffix,
/// or the request path itself when no route matched.
///
/// # Panics
///
/// Never: the caller has already rejected a non-canonical path.
#[must_use]
pub fn outbound_path(resolved: &ResolvedRequest) -> String {
    let Some(route) = resolved.route.as_ref() else {
        return resolved.path_suffix.clone();
    };
    let crate::domain::model::MatchConfig::Http(http) = &route.r#match else {
        return resolved.path_suffix.clone();
    };
    if resolved.path_suffix.is_empty() {
        return http.path.clone();
    }
    if http.path.ends_with('/') {
        return format!("{}{}", http.path, resolved.path_suffix);
    }
    format!("{}/{}", http.path, resolved.path_suffix)
}

/// The upstream-facing WebSocket URL. The transport scheme is HTTP: reqwest
/// upgrades the connection itself, the `ws`/`wss` spelling is the wire contract.
#[must_use]
pub fn websocket_url(endpoint: &Endpoint, path: &str) -> String {
    let scheme = match endpoint.scheme {
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => "https",
        Scheme::Ws | Scheme::Http => "http",
    };
    let host = host_token(&endpoint.host);
    let path = if path.is_empty() {
        "/"
    } else if path.starts_with('/') {
        path
    } else {
        return format!(
            "{scheme}://{host}:{port}/{path}",
            host = host,
            port = endpoint.port
        );
    };
    format!(
        "{scheme}://{host}:{port}{path}",
        host = host,
        port = endpoint.port
    )
}

/// Applies the request-side header transformation to the inbound headers.
///
/// Returns the headers the upstream receives: inbound headers filtered by the
/// passthrough policy, minus the hop-by-hop set, then the `set`/`add` rules.
#[must_use]
pub fn transform_request_headers(
    inbound: &HeaderMap,
    rules: &RequestHeaders,
    connection_headers: &[String],
) -> HeaderMap {
    let mut outbound = HeaderMap::new();
    for (name, value) in inbound {
        let lowered = name.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&lowered, connection_headers) {
            continue;
        }
        if GATEWAY_CONTROL_HEADERS.contains(&lowered.as_str()) {
            continue;
        }
        let forwarded = match rules.passthrough {
            Passthrough::None | Passthrough::Allowlist => rules
                .passthrough_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&lowered)),
            Passthrough::All => true,
        };
        if !forwarded {
            continue;
        }
        if rules
            .remove
            .iter()
            .any(|removed| removed.eq_ignore_ascii_case(&lowered))
        {
            continue;
        }
        if let Ok(parsed) = HeaderName::from_bytes(lowered.as_bytes()) {
            outbound.append(parsed, value.clone());
        }
    }
    for (name, value) in &rules.set {
        insert_header(&mut outbound, name, value);
    }
    for (name, value) in &rules.add {
        if let (Ok(parsed), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            outbound.append(parsed, value);
        }
    }
    outbound
}

/// Applies the response-side header transformation to the upstream headers.
///
/// The upstream's own `Connection` header names hop-by-hop fields, so those are
/// dropped with it; `content-length` goes too because the relay re-frames the
/// body. The gateway's control headers are re-stamped below, never relayed.
#[must_use]
pub fn transform_response_headers(upstream: &HeaderMap, rules: &ResponseHeaders) -> HeaderMap {
    let nominated = connection_named(upstream);
    let mut outbound = HeaderMap::new();
    for (name, value) in upstream {
        let lowered = name.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&lowered, &nominated) || lowered == "content-length" {
            continue;
        }
        if GATEWAY_CONTROL_HEADERS.contains(&lowered.as_str()) {
            continue;
        }
        if rules
            .remove
            .iter()
            .any(|removed| removed.eq_ignore_ascii_case(&lowered))
        {
            continue;
        }
        outbound.append(name.clone(), value.clone());
    }
    for (name, value) in &rules.set {
        insert_header(&mut outbound, name, value);
    }
    for (name, value) in &rules.add {
        if let (Ok(parsed), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            outbound.append(parsed, value);
        }
    }
    outbound
}

/// `true` when a plugin reference points at a custom (stored) plugin, which is
/// executed as pass-through until a Starlark runtime is wired in.
fn is_custom_reference(reference: &str) -> bool {
    uuid::Uuid::parse_str(reference).is_ok()
        || reference
            .rsplit('~')
            .next()
            .is_some_and(|part| uuid::Uuid::parse_str(part).is_ok())
}

fn unknown(reference: &str, kind: &'static str) -> DomainError {
    UnknownPlugin {
        reference: reference.to_owned(),
        kind,
    }
    .into_error()
}

/// Maps a guard rejection onto the OAGW error catalog.
fn guard_error(status: u16, error_code: &'static str, message: String) -> DomainError {
    let detail = format!("{error_code}: {message}");
    match status {
        401 => DomainError::AuthenticationFailed(detail),
        404 => DomainError::RouteNotFound(detail),
        500..=599 => DomainError::DownstreamError(detail),
        _ => DomainError::RouteRejected(detail),
    }
}

/// Executes the plugin chain for one proxy hop.
pub struct PluginExecutor {
    auth: crate::domain::plugin::AuthPluginRegistry,
    guards: GuardPluginRegistry,
    transforms: TransformPluginRegistry,
}

impl std::fmt::Debug for PluginExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PluginExecutor").finish()
    }
}

impl PluginExecutor {
    /// Builds the executor with the built-in plugin registries.
    #[must_use]
    pub fn new(
        resolver: Arc<dyn CredentialResolver>,
        token_cache_ttl: std::time::Duration,
        token_cache_capacity: usize,
    ) -> Self {
        Self {
            auth: auth_registry_with_builtins(resolver, token_cache_ttl, token_cache_capacity),
            guards: crate::infra::plugin::guard_registry_with_builtins(),
            transforms: crate::infra::plugin::transform_registry_with_builtins(),
        }
    }

    /// The request context the plugins act on, primed from the inbound exchange.
    #[must_use]
    pub fn request_context(
        &self,
        method: &str,
        path: &str,
        headers: &HeaderMap,
        tenant_id: &str,
        subject_id: Option<&str>,
    ) -> RequestContext {
        let mut mapped = BTreeMap::new();
        for (name, value) in headers {
            if let Ok(value) = value.to_str() {
                mapped.insert(name.as_str().to_ascii_lowercase(), value.to_owned());
            }
        }
        RequestContext {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: mapped,
            injected_headers: BTreeMap::new(),
            removed_headers: Vec::new(),
            config: serde_json::Value::Null,
            tenant_id: tenant_id.to_owned(),
            subject_id: subject_id.map(str::to_owned),
        }
    }

    /// Runs the auth phase, injecting credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns `AuthenticationFailed`/`SecretNotFound` from the plugin, and
    /// `PluginNotFound` for a reference no built-in satisfies.
    pub async fn authenticate(
        &self,
        reference: &str,
        config: serde_json::Value,
        ctx: &mut RequestContext,
    ) -> DomainResult<()> {
        let plugin = self
            .auth
            .get(reference)
            .ok_or_else(|| unknown(reference, "auth"))?;
        ctx.config = config;
        plugin.authenticate(ctx).await
    }

    /// Runs every request guard in order.
    ///
    /// # Errors
    ///
    /// Returns the first rejection as a domain error.
    pub async fn guard_request(
        &self,
        bindings: &[crate::domain::model::PluginBinding],
        ctx: &RequestContext,
    ) -> DomainResult<()> {
        for binding in bindings {
            let Some(plugin) = self.guards.get(&binding.plugin_ref) else {
                if is_custom_reference(&binding.plugin_ref) {
                    tracing::debug!(
                        plugin = %binding.plugin_ref,
                        "custom guard plugin has no runtime binding; executing as pass-through"
                    );
                    continue;
                }
                return Err(unknown(&binding.plugin_ref, "guard"));
            };
            let mut scoped = ctx.clone();
            scoped.config = binding.config.clone();
            match plugin.guard_request(&scoped).await? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } => return Err(guard_error(status, error_code, message)),
            }
        }
        Ok(())
    }

    /// Runs every request transform in order.
    ///
    /// # Errors
    ///
    /// Returns the plugin's error.
    pub async fn transform_request(
        &self,
        bindings: &[crate::domain::model::PluginBinding],
        ctx: &mut RequestContext,
    ) -> DomainResult<()> {
        for binding in bindings {
            let Some(plugin) = self.transforms.get(&binding.plugin_ref) else {
                if is_custom_reference(&binding.plugin_ref) {
                    tracing::debug!(
                        plugin = %binding.plugin_ref,
                        "custom transform plugin has no runtime binding; executing as pass-through"
                    );
                    continue;
                }
                return Err(unknown(&binding.plugin_ref, "transform"));
            };
            let mut scoped = ctx.clone();
            scoped.config = binding.config.clone();
            plugin.transform_request(&mut scoped).await?;
            for (name, value) in std::mem::take(&mut scoped.injected_headers) {
                ctx.injected_headers.insert(name, value);
            }
            ctx.removed_headers
                .extend(std::mem::take(&mut scoped.removed_headers));
        }
        Ok(())
    }

    /// Runs every response guard and transform in order.
    ///
    /// # Errors
    ///
    /// Returns the plugin's error.
    pub async fn transform_response(
        &self,
        bindings: &[crate::domain::model::PluginBinding],
        ctx: &mut ResponseContext,
    ) -> DomainResult<()> {
        for binding in bindings {
            let mut scoped = ctx.clone();
            scoped.config = binding.config.clone();
            if let Some(plugin) = self.guards.get(&binding.plugin_ref) {
                if let GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } = plugin.guard_response(&scoped).await?
                {
                    return Err(guard_error(status, error_code, message));
                }
                continue;
            }
            let Some(plugin) = self.transforms.get(&binding.plugin_ref) else {
                if is_custom_reference(&binding.plugin_ref) {
                    tracing::debug!(
                        plugin = %binding.plugin_ref,
                        "custom response plugin has no runtime binding; executing as pass-through"
                    );
                }
                continue;
            };
            plugin.transform_response(&mut scoped).await?;
            for (name, value) in std::mem::take(&mut scoped.injected_headers) {
                ctx.injected_headers.insert(name, value);
            }
        }
        Ok(())
    }
}

/// The proxy engine over an [`OutboundClient`].
pub struct ProxyEngine {
    client: OutboundClient,
    plugins: Arc<PluginExecutor>,
    config: Arc<OagwConfig>,
    metrics: Option<SharedMetrics>,
}

impl std::fmt::Debug for ProxyEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ProxyEngine").finish()
    }
}

impl ProxyEngine {
    /// Creates the engine.
    #[must_use]
    pub fn new(
        client: OutboundClient,
        plugins: Arc<PluginExecutor>,
        config: Arc<OagwConfig>,
    ) -> Self {
        Self {
            client,
            plugins,
            config,
            metrics: None,
        }
    }

    /// Attaches the data-plane metrics so routing decisions are observable.
    #[must_use]
    pub fn with_metrics(mut self, metrics: SharedMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// The transport client (used by the WebSocket tunnel).
    #[must_use]
    pub const fn client(&self) -> &OutboundClient {
        &self.client
    }

    /// The plugin executor.
    #[must_use]
    pub const fn plugins(&self) -> &Arc<PluginExecutor> {
        &self.plugins
    }

    /// The gear configuration.
    #[must_use]
    pub const fn config(&self) -> &Arc<OagwConfig> {
        &self.config
    }

    /// Runs one proxy hop and returns the client-facing response.
    ///
    /// # Errors
    ///
    /// Returns the routing, plugin and transport errors of the flow.
    pub async fn forward(
        &self,
        resolved: &ResolvedRequest,
        method: &http::Method,
        inbound_headers: &HeaderMap,
        body: Option<Bytes>,
        tenant_id: &str,
        subject_id: Option<&str>,
    ) -> DomainResult<Response> {
        let path = outbound_path(resolved);
        let endpoint = self.dial_target(resolved, inbound_headers)?;
        let url = outbound_url(endpoint, &with_query(&path, resolved.query.as_deref()));

        let connection_headers = connection_named(inbound_headers);
        let mut ctx = self.plugins.request_context(
            method.as_str(),
            &path,
            inbound_headers,
            tenant_id,
            subject_id,
        );
        if let Some(reference) = resolved.auth.plugin_type.as_deref()
            && let Err(error) = self
                .plugins
                .authenticate(reference, resolved.auth.config.clone(), &mut ctx)
                .await
        {
            // DESIGN §4.3: record the failure, sampled so a misconfigured
            // upstream cannot flood the audit log.
            audit::auth_failure(
                tenant_id,
                &endpoint.host,
                reference,
                audit::sample_auth_failure(),
            );
            return Err(error);
        }
        self.plugins
            .guard_request(&resolved.plugins.items, &ctx)
            .await?;
        self.plugins
            .transform_request(&resolved.plugins.items, &mut ctx)
            .await?;

        let mut outbound = transform_request_headers(
            inbound_headers,
            &resolved.headers.request,
            &connection_headers,
        );
        for (name, value) in &ctx.injected_headers {
            insert_header(&mut outbound, name, value);
        }
        for name in &ctx.removed_headers {
            if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
                outbound.remove(parsed);
            }
        }
        insert_header(
            &mut outbound,
            "host",
            &format!("{}:{}", endpoint.host, endpoint.port),
        );

        let request = OutboundRequest {
            url,
            method: method.clone(),
            headers: outbound,
            body,
            timeout: self.config.proxy_timeout(),
        };
        let response = self.client.send(request).await?;

        let mut response_ctx = ResponseContext {
            status: response.status_code(),
            headers: response
                .headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
                })
                .collect(),
            injected_headers: BTreeMap::new(),
            config: serde_json::Value::Null,
        };
        self.plugins
            .transform_response(&resolved.plugins.items, &mut response_ctx)
            .await?;

        let mut headers = transform_response_headers(&response.headers, &resolved.headers.response);
        for (name, value) in &response_ctx.injected_headers {
            insert_header(&mut headers, name, value);
        }
        apply_cors(&mut headers, resolved.cors.as_ref(), inbound_headers);
        attach_identity_headers(&mut headers, resolved);
        mark_upstream_errors(&mut headers, response.status_code());

        let status = axum::http::StatusCode::from_u16(response.status_code())
            .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
        let streaming = response.inner;
        let content_type = response
            .headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if is_event_stream(content_type.as_deref()) {
            return Ok(stream_response(status, headers, streaming));
        }
        Ok(buffered_response(status, headers, streaming, self.config.max_body_size_bytes).await)
    }

    /// Tunnels a WebSocket upgrade to the upstream and returns the raw
    /// byte-level bridge both endpoints talk through.
    ///
    /// # Errors
    ///
    /// Returns a routing/transport error, or `ProtocolError` when the upstream
    /// declines the upgrade.
    pub async fn forward_websocket(
        &self,
        resolved: &ResolvedRequest,
        inbound_headers: &HeaderMap,
        path: &str,
        tenant_id: &str,
        subject_id: Option<&str>,
    ) -> DomainResult<tokio_tungstenite::WebSocketStream<reqwest::Upgraded>> {
        let endpoint = self.dial_target(resolved, inbound_headers)?;
        let url = websocket_url(endpoint, &with_query(path, resolved.query.as_deref()));
        let mut headers = transform_request_headers(
            inbound_headers,
            &resolved.headers.request,
            &connection_named(inbound_headers),
        );
        headers.remove(reqwest::header::SEC_WEBSOCKET_KEY);
        headers.remove(reqwest::header::SEC_WEBSOCKET_ACCEPT);
        headers.remove(reqwest::header::SEC_WEBSOCKET_VERSION);
        // The tunnel speaks raw WebSocket frames; a negotiated compression
        // extension would make us promise a codec we do not implement.
        headers.remove("sec-websocket-extensions");
        headers.remove(reqwest::header::SEC_WEBSOCKET_PROTOCOL);
        let mut ctx =
            self.plugins
                .request_context("GET", path, inbound_headers, tenant_id, subject_id);
        if let Some(reference) = resolved.auth.plugin_type.as_deref()
            && let Err(error) = self
                .plugins
                .authenticate(reference, resolved.auth.config.clone(), &mut ctx)
                .await
        {
            // DESIGN §4.3: record the failure, sampled so a misconfigured
            // upstream cannot flood the audit log.
            audit::auth_failure(
                tenant_id,
                &endpoint.host,
                reference,
                audit::sample_auth_failure(),
            );
            return Err(error);
        }
        self.plugins
            .guard_request(&resolved.plugins.items, &ctx)
            .await?;
        self.plugins
            .transform_request(&resolved.plugins.items, &mut ctx)
            .await?;
        for (name, value) in &ctx.injected_headers {
            insert_header(&mut headers, name, value);
        }
        for name in &ctx.removed_headers {
            if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
                headers.remove(parsed);
            }
        }
        insert_header(&mut headers, "connection", "Upgrade");
        insert_header(&mut headers, "upgrade", "websocket");
        let upgraded = self
            .client
            .connect_websocket(url, headers, self.config.proxy_timeout())
            .await?;
        // reqwest hands back the raw upgraded stream; the client side of the
        // WebSocket framing is ours to speak.
        let tunnel = tokio_tungstenite::WebSocketStream::from_raw_socket(
            upgraded,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        Ok(tunnel)
    }

    /// Resolves and re-checks the dial target just before the hop (ADR-0007).
    fn dial_target<'a>(
        &self,
        resolved: &'a ResolvedRequest,
        inbound_headers: &HeaderMap,
    ) -> DomainResult<&'a Endpoint> {
        let pinned = inbound_headers
            .get(TARGET_HOST_HEADER)
            .and_then(|value| value.to_str().ok());
        let endpoint = resolved.target_endpoint(pinned)?;
        if let Some(metrics) = self.metrics.as_ref() {
            // DESIGN §4.2: how the pool member was chosen.
            metrics.record_endpoint_selection(
                &resolved.upstream.id.to_string(),
                &format!("{}:{}", endpoint.host, endpoint.port),
                if pinned.is_some() {
                    "explicit_header"
                } else if resolved.upstream.server.endpoints.len() > 1 {
                    "round_robin"
                } else {
                    "default"
                },
            );
        }
        if endpoint.scheme.is_plaintext() && !self.config.plaintext_upstreams_allowed() {
            return Err(DomainError::ProtocolError(format!(
                "endpoint {}:{} uses a plaintext scheme the configuration forbids",
                endpoint.host, endpoint.port
            )));
        }
        ssrf::check_host(&endpoint.host, &self.config)?;
        Ok(endpoint)
    }
}

/// Adds the CORS headers the merged policy grants for this origin (ADR-0004).
fn apply_cors(
    headers: &mut HeaderMap,
    config: Option<&crate::domain::model::CorsConfig>,
    inbound: &HeaderMap,
) {
    let Some(config) = config else {
        return;
    };
    let origin = inbound
        .get(cors::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let decision: SimpleCors = cors::evaluate(config, origin);
    decision.apply(headers);
}

/// Adds the routing identity headers the API contract documents.
fn attach_identity_headers(headers: &mut HeaderMap, resolved: &ResolvedRequest) {
    insert_header(headers, UPSTREAM_ID_HEADER, &resolved.upstream.gts_id());
    if let Some(route) = resolved.route.as_ref() {
        insert_header(headers, ROUTE_ID_HEADER, &route.gts_id());
    }
}

/// Marks a relayed upstream failure as coming from the upstream (ADR-0007).
///
/// The upstream body and status are preserved verbatim; the header is what tells
/// the caller whether the `4xx`/`5xx` they are reading was produced by the
/// gateway or merely relayed through it.
fn mark_upstream_errors(headers: &mut HeaderMap, status: u16) {
    if status < 400 {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(ERROR_SOURCE_UPSTREAM) {
        headers.insert(HeaderName::from_static("x-oagw-error-source"), value);
    }
}

/// Builds the SSE/streaming response.
fn stream_response(
    status: axum::http::StatusCode,
    headers: HeaderMap,
    upstream: reqwest::Response,
) -> Response {
    let stream = upstream.bytes_stream().map(|chunk| match chunk {
        Ok(bytes) => Ok::<Bytes, std::io::Error>(bytes),
        Err(error) => Err(std::io::Error::other(error)),
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Buffers a non-streaming upstream response, refusing bodies past `limit`.
///
/// The cap is what keeps a misbehaving upstream from pinning the gateway's
/// memory with an unbounded body; an event stream never takes this path.
async fn buffered_response(
    status: axum::http::StatusCode,
    headers: HeaderMap,
    upstream: reqwest::Response,
    limit: u64,
) -> Response {
    let mut stream = upstream.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return crate::api::error::OagwError::from(DomainError::DownstreamError(
                    error.to_string(),
                ))
                .into_response();
            }
        };
        let projected = body.len().saturating_add(chunk.len()) as u64;
        if projected > limit {
            return crate::api::error::OagwError::from(DomainError::PayloadTooLarge { limit })
                .into_response();
        }
        body.extend_from_slice(&chunk);
    }
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Renders an upstream failure as a client response (`X-OAGW-Error-Source:
/// upstream`, ADR-0007).
#[must_use]
pub fn upstream_response(status: u16, body: String) -> Response {
    upstream_error(status, body)
}

/// Re-exported for handlers that need to name the error source.
pub const ERROR_SOURCE: &str = ERROR_SOURCE_UPSTREAM;

/// `true` when the inbound exchange asks for a WebSocket tunnel.
#[must_use]
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    is_websocket_upgrade(headers)
}

/// Re-exports the request-id transform for gear wiring.
pub type DefaultTransform = RequestIdTransformPlugin;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, HeadersConfig, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, PluginBinding,
        PluginsConfig, Route, ServerConfig,
    };
    use std::collections::BTreeMap;

    fn endpoint(host: &str, port: u16, scheme: Scheme) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn route(path: &str) -> Route {
        Route {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            upstream_id: Some(uuid::Uuid::new_v4()),
            r#match: MatchConfig::Http(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            priority: 0,
            enabled: true,
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn upstream(
        alias: &str,
        host: &str,
        port: u16,
        scheme: Scheme,
    ) -> crate::domain::model::Upstream {
        crate::domain::model::Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            alias: alias.to_owned(),
            protocol: crate::domain::model::Protocol::Http,
            enabled: true,
            server: ServerConfig {
                endpoints: vec![endpoint(host, port, scheme)],
            },
            auth: crate::domain::model::AuthConfig::default(),
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn outbound_urls_carry_scheme_host_port_and_path() {
        let endpoint = endpoint("api.example", 8443, Scheme::Https);
        assert_eq!(
            outbound_url(&endpoint, "/v1/chat"),
            "https://api.example:8443/v1/chat"
        );
        assert_eq!(outbound_url(&endpoint, ""), "https://api.example:8443/");
        assert_eq!(outbound_url(&endpoint, "v1"), "https://api.example:8443/v1");
    }

    #[test]
    fn websocket_urls_carry_the_transport_scheme() {
        // `ws`/`wss` name the wire vocabulary; the dial itself must be http(s)
        // because the outbound client speaks HTTP until the 101 upgrades it.
        assert_eq!(
            websocket_url(&endpoint("s.example", 80, Scheme::Ws), "/ws"),
            "http://s.example:80/ws"
        );
        assert_eq!(
            websocket_url(&endpoint("s.example", 443, Scheme::Wss), "/ws"),
            "https://s.example:443/ws"
        );
        assert_eq!(
            websocket_url(&endpoint("s.example", 443, Scheme::Https), "/ws"),
            "https://s.example:443/ws"
        );
    }

    #[test]
    fn event_streams_are_detected_by_content_type() {
        assert!(is_event_stream(Some("text/event-stream")));
        assert!(is_event_stream(Some("text/event-stream; charset=utf-8")));
        assert!(!is_event_stream(Some("application/json")));
        assert!(!is_event_stream(None));
    }

    #[test]
    fn request_header_passthrough_none_keeps_only_the_allowlist() {
        let mut inbound = HeaderMap::new();
        insert_header(&mut inbound, "authorization", "Bearer client-token");
        insert_header(&mut inbound, "x-internal", "secret");
        insert_header(&mut inbound, "x-forwarded-for", "10.0.0.1");
        insert_header(&mut inbound, "connection", "close");
        let rules = RequestHeaders {
            passthrough: Passthrough::None,
            passthrough_allowlist: vec!["x-internal".to_owned()],
            remove: vec![],
            set: BTreeMap::from([("x-gateway".to_owned(), "oagw".to_owned())]),
            add: BTreeMap::from([("x-trace".to_owned(), "1".to_owned())]),
        };
        let outbound = transform_request_headers(&inbound, &rules, &[]);
        assert!(outbound.contains_key("x-internal"));
        assert!(!outbound.contains_key("authorization"));
        assert!(!outbound.contains_key("x-forwarded-for"));
        assert!(!outbound.contains_key("connection"));
        assert_eq!(outbound.get("x-gateway").unwrap(), "oagw");
        assert!(outbound.contains_key("x-trace"));
    }

    #[test]
    fn request_header_passthrough_all_forwards_everything_but_hop_by_hop() {
        let mut inbound = HeaderMap::new();
        insert_header(&mut inbound, "authorization", "Bearer client-token");
        insert_header(&mut inbound, "connection", "X-Drop");
        insert_header(&mut inbound, "x-drop", "value");
        let rules = RequestHeaders {
            passthrough: Passthrough::All,
            ..RequestHeaders::default()
        };
        let outbound = transform_request_headers(&inbound, &rules, &connection_named(&inbound));
        assert!(outbound.contains_key("authorization"));
        assert!(!outbound.contains_key("x-drop"));
        assert!(!outbound.contains_key("connection"));
    }

    #[test]
    fn remove_rules_beat_the_allowlist() {
        let mut inbound = HeaderMap::new();
        insert_header(&mut inbound, "x-secret", "value");
        let rules = RequestHeaders {
            passthrough: Passthrough::All,
            remove: vec!["X-Secret".to_owned()],
            ..RequestHeaders::default()
        };
        let outbound = transform_request_headers(&inbound, &rules, &[]);
        assert!(!outbound.contains_key("x-secret"));
    }

    #[test]
    fn response_headers_drop_hop_by_hop_and_apply_rules() {
        let mut upstream = HeaderMap::new();
        insert_header(&mut upstream, "content-type", "application/json");
        insert_header(&mut upstream, "transfer-encoding", "chunked");
        insert_header(&mut upstream, "x-secret", "no");
        let rules = ResponseHeaders {
            set: BTreeMap::from([("x-served-by".to_owned(), "oagw".to_owned())]),
            add: BTreeMap::new(),
            remove: vec!["x-secret".to_owned()],
        };
        let outbound = transform_response_headers(&upstream, &rules);
        assert!(outbound.contains_key("content-type"));
        assert!(!outbound.contains_key("transfer-encoding"));
        assert!(!outbound.contains_key("x-secret"));
        assert_eq!(outbound.get("x-served-by").unwrap(), "oagw");
    }

    #[test]
    fn outbound_path_joins_the_route_prefix_and_suffix() {
        let owner = upstream("api.example:8080", "api.example", 8080, Scheme::Http);
        let mut resolved = crate::domain::services::proxy::effective(
            &owner,
            Some(&route("/v1/chat")),
            "completions".to_owned(),
        );
        assert_eq!(outbound_path(&resolved), "/v1/chat/completions");
        resolved.path_suffix = String::new();
        assert_eq!(outbound_path(&resolved), "/v1/chat");
        resolved.route = None;
        assert_eq!(outbound_path(&resolved), "");
    }

    #[test]
    fn the_admitted_query_joins_the_outbound_path() {
        assert_eq!(
            with_query("/v1/models", Some("limit=5")),
            "/v1/models?limit=5"
        );
        assert_eq!(with_query("/v1/models", Some("")), "/v1/models");
        assert_eq!(with_query("/v1/models", None), "/v1/models");
    }

    #[test]
    fn gateway_control_headers_never_cross_the_hop() {
        let mut inbound = HeaderMap::new();
        for name in [
            "x-oagw-target-host",
            "x-oagw-client-ip",
            "x-oagw-error-source",
            "x-oagw-upstream-id",
            "x-oagw-route-id",
            "x-keep",
        ] {
            insert_header(&mut inbound, name, "value");
        }
        let rules = RequestHeaders {
            passthrough: Passthrough::All,
            ..RequestHeaders::default()
        };
        let outbound = transform_request_headers(&inbound, &rules, &[]);
        assert!(outbound.contains_key("x-keep"));
        for name in GATEWAY_CONTROL_HEADERS {
            assert!(outbound.get(name).is_none(), "{name} must not be forwarded");
        }
    }

    #[test]
    fn an_upstream_error_source_is_not_relayed() {
        let mut upstream = HeaderMap::new();
        insert_header(&mut upstream, "x-oagw-error-source", "upstream");
        insert_header(&mut upstream, "connection", "X-Private");
        insert_header(&mut upstream, "x-private", "1");
        insert_header(&mut upstream, "x-keep", "1");
        let outbound = transform_response_headers(&upstream, &ResponseHeaders::default());
        assert!(outbound.get("x-oagw-error-source").is_none());
        assert!(outbound.get("x-private").is_none());
        assert!(outbound.get("x-keep").is_some());
    }

    #[test]
    fn traversal_segments_are_rejected() {
        assert!(is_canonical_path("/v1/models"));
        assert!(!is_canonical_path("/v1/%2e%2e/etc"));
        assert!(!is_canonical_path("/v1/../etc/passwd"));
        assert!(!is_canonical_path("/v1/./etc"));
        assert!(!is_canonical_path("/v1/%2e%2e/passwd"));
    }

    #[test]
    fn custom_plugin_references_run_as_pass_through() {
        let custom = uuid::Uuid::new_v4().to_string();
        assert!(is_custom_reference(&custom));
        assert!(is_custom_reference(&format!(
            "gts.cf.core.oagw.transform_plugin.v1~{custom}"
        )));
        assert!(!is_custom_reference(
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1"
        ));
    }

    #[test]
    fn unknown_named_plugins_are_reported() {
        let error = unknown(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
            "guard",
        );
        assert_eq!(error.status_code(), 400);
        let error = unknown(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.madeup.v1",
            "auth",
        );
        assert_eq!(error.status_code(), 503);
    }

    #[test]
    fn guard_rejections_map_onto_the_error_catalog() {
        assert_eq!(
            guard_error(400, "route_rejected", "no".to_owned()).status_code(),
            400
        );
        assert_eq!(guard_error(401, "auth", "no".to_owned()).status_code(), 401);
        assert_eq!(
            guard_error(500, "guard", "no".to_owned()).status_code(),
            502
        );
    }

    #[test]
    fn cors_headers_are_applied_from_the_merged_policy() {
        let mut headers = HeaderMap::new();
        let mut inbound = HeaderMap::new();
        insert_header(&mut inbound, "origin", "https://app.example");
        let policy = crate::domain::model::CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example".to_owned()],
            ..crate::domain::model::CorsConfig::default()
        };
        apply_cors(&mut headers, Some(&policy), &inbound);
        assert_eq!(
            headers.get("access-control-allow-origin").unwrap(),
            "https://app.example"
        );
        let mut bare = HeaderMap::new();
        apply_cors(&mut bare, None, &inbound);
        assert!(bare.is_empty());
        // A policy that is off emits nothing.
        let mut disabled = HeaderMap::new();
        apply_cors(
            &mut disabled,
            Some(&crate::domain::model::CorsConfig::default()),
            &inbound,
        );
        assert!(disabled.is_empty());
    }

    #[test]
    fn identity_headers_carry_the_gts_ids() {
        let owner = upstream("api.example:8080", "api.example", 8080, Scheme::Http);
        let matched = route("/v1");
        let resolved =
            crate::domain::services::proxy::effective(&owner, Some(&matched), String::new());
        let mut headers = HeaderMap::new();
        attach_identity_headers(&mut headers, &resolved);
        assert_eq!(headers.get(UPSTREAM_ID_HEADER).unwrap(), &owner.gts_id());
        assert_eq!(headers.get(ROUTE_ID_HEADER).unwrap(), &matched.gts_id());

        let unresolved = crate::domain::services::proxy::effective(&owner, None, String::new());
        let mut headers = HeaderMap::new();
        attach_identity_headers(&mut headers, &unresolved);
        assert!(headers.get(ROUTE_ID_HEADER).is_none());
    }

    #[test]
    fn plugin_bindings_are_accepted_as_references_or_objects() {
        let bindings: Vec<PluginBinding> = serde_json::from_value(serde_json::json!(
            [
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
                {
                    "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                    "config": {"required": ["x-request-id"]}
                }
            ]
        ))
        .expect("bindings");
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].config, serde_json::Value::Null);
        assert!(bindings[1].config.is_object());
    }
}
