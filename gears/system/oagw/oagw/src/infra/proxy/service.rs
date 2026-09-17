//! Data plane: proxy request orchestration.
//!
//! Resolves the upstream and route from the control plane, applies the
//! plugin chain, and forwards the request over a dedicated upstream
//! connection. Streaming (SSE) and WebSocket upgrades are proxied
//! transparently without buffering.

use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http::header::{HeaderMap, HeaderValue};
use http::{Method, StatusCode};
use hyper::body::{Frame, Incoming};

use crate::config::OagwConfig;
use crate::domain::circuit_breaker::{CircuitBreaker, SharedBreaker};
use crate::domain::model::{
    Endpoint, HeaderRules, MatchRule, PassthroughMode, Route, Upstream,
};
use crate::domain::rate_limit::{limited_error, rate_key, RateLimiter, SharedRateLimiter};
use crate::domain::routing;
use crate::domain::service::{ControlPlaneService, TenantHierarchy};
use crate::error::{ErrorKind, OagwError};
use crate::infra::headers as header_ops;
use crate::infra::plugins::{
    execute_request, PluginChain, PluginRegistry, PluginRequest,
};
use crate::infra::proxy::transport::{self, Target, TlsSettings, UpstreamConnection};

/// Outbound body type. Re-uses the inbound axum body so streaming uploads
/// and protocol upgrades stay streaming end to end.
pub type ProxyBody = axum::body::Body;

/// Result of a proxy execution.
#[derive(Debug)]
pub enum ProxyOutcome {
    /// An upstream response to pass through to the caller.
    Pass {
        /// Upstream status.
        status: StatusCode,
        /// Upstream headers, after configured response rules.
        headers: HeaderMap,
        /// Streaming upstream body with idle-timeout enforcement.
        body: axum::body::Body,
        /// Correlation identifier echoed back to the client.
        request_id: Option<String>,
    },
    /// A completed protocol upgrade (WebSocket): the response is already
    /// 101 and the splice task is running.
    Upgrade {
        /// The 101 response to hand back to axum.
        response: axum::response::Response,
        /// Correlation identifier.
        request_id: Option<String>,
    },
}

/// Data-plane orchestration service.
pub struct DataPlaneService<U, R, P> {
    control: Arc<ControlPlaneService<U, R, P>>,
    hierarchy: Arc<dyn TenantHierarchy>,
    registry: Arc<PluginRegistry>,
    secrets: crate::infra::secrets::SecretResolver,
    rate_limiter: SharedRateLimiter,
    breakers: SharedBreaker,
    tls: Option<TlsSettings>,
    config: OagwConfig,
    round_robin: AtomicUsize,
}

/// Type alias for the in-memory wired data plane.
pub type SharedDataPlaneService = DataPlaneService<
    crate::infra::storage::MemoryStore,
    crate::infra::storage::MemoryStore,
    crate::infra::storage::MemoryStore,
>;

impl<U, R, P> DataPlaneService<U, R, P>
where
    U: crate::domain::repo::UpstreamRepository + 'static,
    R: crate::domain::repo::RouteRepository + 'static,
    P: crate::domain::repo::PluginRepository + 'static,
{
    /// Wire a data plane over a control plane.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        control: Arc<ControlPlaneService<U, R, P>>,
        hierarchy: Arc<dyn TenantHierarchy>,
        registry: Arc<PluginRegistry>,
        secrets: crate::infra::secrets::SecretResolver,
        tls: Option<TlsSettings>,
        config: OagwConfig,
    ) -> Self {
        Self {
            control,
            hierarchy,
            registry,
            secrets,
            rate_limiter: Arc::new(RateLimiter::new()),
            breakers: Arc::new(CircuitBreaker::new()),
            tls,
            config,
            round_robin: AtomicUsize::new(0),
        }
    }

    /// Access the underlying control plane.
    #[must_use]
    pub fn control_plane(&self) -> &Arc<ControlPlaneService<U, R, P>> {
        &self.control
    }

    /// Execute a proxy request end to end.
    ///
    /// # Errors
    /// Returns the RFC 9457 gateway error for every failure mode in
    /// `DESIGN.md §3.3`.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        ctx: &toolkit_security::SecurityContext,
        alias: &str,
        path_suffix: &str,
        raw_query: Option<&str>,
        method: &Method,
        mut headers: HeaderMap,
        body: ProxyBody,
        client_ip: Option<IpAddr>,
        request_id: Option<String>,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Result<ProxyOutcome, OagwError> {
        let started = Instant::now();
        let tenant_id = ctx.subject_tenant_id();
        let target_host_header = headers
            .get("x-oagw-target-host")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        // 1. Resolve the upstream across the tenant chain (closest wins).
        let upstream = self
            .control
            .resolve_upstream(self.hierarchy.as_ref(), tenant_id, alias)
            .await?;
        if !upstream.enabled {
            return Err(OagwError::new(
                ErrorKind::UpstreamDisabled,
                format!("upstream '{alias}' is disabled"),
            )
            .with_ext("upstream_id", upstream.id.to_string())
            .with_ext("host", alias));
        }

        let breaker_key = upstream.id.to_string();
        self.breakers.before(&breaker_key)?;

        // 2. Rate limiting: upstream level, then route level.
        if let Some(limit) = &upstream.rate_limit {
            let key = rate_key(
                upstream.id,
                None,
                limit.scope,
                tenant_id,
                ctx.subject_id(),
                client_ip,
            );
            if let Some(err) = limited_error(self.rate_limiter.check(limit, &key)) {
                return Err(err);
            }
        }

        // 3. Route matching (method allowlist + longest prefix).
        let route = self
            .control
            .resolve_route(
                self.hierarchy.as_ref(),
                tenant_id,
                upstream.id,
                method,
                path_suffix,
            )
            .await?;

        if let Some(limit) = &route.rate_limit {
            let key = rate_key(
                upstream.id,
                Some(route.id),
                limit.scope,
                tenant_id,
                ctx.subject_id(),
                client_ip,
            );
            if let Some(err) = limited_error(self.rate_limiter.check(limit, &key)) {
                return Err(err);
            }
        }

        // 4. Endpoint selection (round-robin or `X-OAGW-Target-Host` pinning).
        let endpoint = self.select_endpoint(&upstream, target_host_header.as_deref())?;

        // 5. Path + query construction.
        let target_path = match &route.r#match {
            MatchRule::Http(http) => {
                routing::build_target_path(&http.path, http.path_suffix_mode, path_suffix)?
            }
            MatchRule::Grpc(_) => {
                return Err(OagwError::new(
                    ErrorKind::RouteError,
                    "gRPC proxying is not implemented in this release",
                )
                .with_ext("status_override", 501u64));
            }
        };
        let mut path_and_query = target_path;
        if let Some(query) = raw_query.filter(|q| !q.is_empty()) {
            let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if let MatchRule::Http(http) = &route.r#match {
                routing::check_query_allowlist(
                    &http.query_allowlist,
                    &pairs
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect::<Vec<_>>(),
                )?;
            }
            path_and_query = format!("{path_and_query}?{query}");
        }

        // 6. CORS on actual (non-preflight) requests.
        if let Some(origin) = headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        {
            let cors = route.cors.clone().or_else(|| upstream.cors.clone());
            if let Some(cors) = cors.filter(|c| c.enabled) {
                if !cors.is_origin_allowed(origin) {
                    return Err(OagwError::new(
                        ErrorKind::CorsOriginNotAllowed,
                        format!("origin '{origin}' is not allowed for this route"),
                    )
                    .with_ext("host", alias));
                }
                if !cors.is_method_allowed(method) {
                    return Err(OagwError::new(
                        ErrorKind::CorsMethodNotAllowed,
                        format!("method {method} is not allowed for this route"),
                    )
                    .with_ext("host", alias));
                }
            }
        }

        // 7. Body validation before any buffering.
        if let Some(len) = headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|len| *len > self.config.max_body_bytes)
        {
            return Err(OagwError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "request payload of {len} bytes exceeds the {} byte limit",
                    self.config.max_body_bytes
                ),
            )
            .with_ext("upstream_id", upstream.id.to_string()));
        }
        let is_websocket = is_websocket_upgrade(&headers);
        // Only `chunked` can be relayed; any other transfer encoding would be
        // replayed verbatim against an upstream that cannot decode it.
        let unsupported_te = !is_websocket
            && headers
                .get(http::header::TRANSFER_ENCODING)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|te| !te.eq_ignore_ascii_case("chunked"));
        if unsupported_te {
            return Err(OagwError::new(
                ErrorKind::TransferEncodingUnsupported,
                format!(
                    "transfer encoding '{}' is not supported; only 'chunked' is",
                    headers
                        .get(http::header::TRANSFER_ENCODING)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                ),
            ));
        }

        // 8. Header transformation.
        let effective_headers = merge_headers(&upstream, &route);
        // The upgrade handshake rides hop-by-hop fields; it has to be
        // re-applied after the strip or the upstream never sees the key.
        let upgrade_handshake = is_websocket.then(|| header_ops::capture_upgrade_headers(&headers));
        header_ops::strip_hop_by_hop(&mut headers);
        let mut outbound = header_ops::apply_passthrough(&headers, &effective_headers.request);
        header_ops::apply_request_rules(&mut outbound, &effective_headers.request);
        if let Some(handshake) = upgrade_handshake.as_ref() {
            header_ops::restore_upgrade_headers(&mut outbound, handshake);
        }
        if let Ok(host_value) =
            HeaderValue::from_str(&transport::host_header(&endpoint))
        {
            outbound.insert(http::header::HOST, host_value);
        }

        // 9. Plugin chain: Auth → Guards → Transform(request).
        let chain = PluginChain::build(
            self.registry.as_ref(),
            &upstream.plugins.items,
            &route.plugins.items,
        );
        let mut auth_refs: Vec<String> = upstream
            .auth
            .as_ref()
            .and_then(|a| a.config_str("secret_ref"))
            .map(|r| vec![r.to_owned()])
            .unwrap_or_default();
        for binding in upstream.plugins.items.iter().chain(route.plugins.items.iter()) {
            if let Some(reference) = binding
                .config
                .get("secret_ref")
                .and_then(serde_json::Value::as_str)
            {
                auth_refs.push(reference.to_owned());
            }
        }
        auth_refs.dedup();
        let secrets = self.secrets.resolve_auth_refs(&auth_refs, tenant_id).await;

        let mut plugin_request = PluginRequest {
            headers: outbound,
            meta: Default::default(),
            secrets,
        };
        execute_request(
            self.registry.as_ref(),
            &chain,
            &mut plugin_request,
            upstream.auth.as_ref(),
        )
        .await?;

        let request_id = plugin_request.meta.get("request_id").cloned().or(request_id);

        // 10. Forward to the selected endpoint.
        let target = Target {
            scheme: endpoint.scheme.clone(),
            host: endpoint.host.clone(),
            port: endpoint.port,
        };
        let outcome = self
            .forward(
                &target,
                method,
                &path_and_query,
                plugin_request.headers,
                body,
                is_websocket,
                client_upgrade,
                &request_id,
            )
            .await;

        match outcome {
            Ok(resp) => {
                self.breakers.success(&breaker_key);
                tracing::info!(
                    upstream = %upstream.alias,
                    path = %path_and_query,
                    status = ?outcome_status(&resp),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "proxied request"
                );
                Ok(resp)
            }
            Err(err) => {
                self.breakers.failure(&breaker_key);
                Err(err)
            }
        }
    }

    /// Select the endpoint to dial for this request.
    fn select_endpoint(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<Endpoint, OagwError> {
        let endpoints = &upstream.server.endpoints;
        let first = endpoints.first().ok_or_else(|| {
            OagwError::new(ErrorKind::LinkUnavailable, "upstream has no configured endpoints")
        })?;
        if let Some(host) = target_host {
            let host = crate::domain::validation::normalize_alias(host);
            return endpoints
                .iter()
                .find(|e| e.host.eq_ignore_ascii_case(&host))
                .cloned()
                .ok_or_else(|| {
                    OagwError::new(
                        ErrorKind::UnknownTargetHost,
                        format!(
                            "X-OAGW-Target-Host '{host}' does not match any configured endpoint"
                        ),
                    )
                    .with_ext("host", host)
                });
        }
        if endpoints.len() > 1 {
            let alias_is_specific = endpoints
                .iter()
                .any(|e| crate::domain::validation::normalize_alias(&e.host) == upstream.alias);
            if !alias_is_specific {
                // Alias resolved to a shared suffix: the caller must pin the
                // endpoint when it cares which member serves the request.
                return Err(OagwError::new(
                    ErrorKind::MissingTargetHost,
                    "X-OAGW-Target-Host is required to select an endpoint of this multi-endpoint upstream",
                )
                .with_ext("upstream_id", upstream.id.to_string()));
            }
        }
        let index = self
            .round_robin
            .fetch_add(1, Ordering::Relaxed)
            % endpoints.len();
        Ok(if index == 0 { first.clone() } else { endpoints[index].clone() })
    }

    /// Build the outbound request and dispatch it.
    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        target: &Target,
        method: &Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: ProxyBody,
        is_websocket: bool,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
        request_id: &Option<String>,
    ) -> Result<ProxyOutcome, OagwError> {
        let uri = transport::build_uri(target, path_and_query);
        let mut builder = http::Request::builder().method(method.clone()).uri(uri);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(body)
            .map_err(|err| {
                OagwError::new(ErrorKind::ValidationError, format!("invalid outbound request: {err}"))
            })?;

        let conn: UpstreamConnection = transport::connect(
            target,
            self.tls.as_ref(),
            self.config.connect_timeout(),
            &target.host,
        )
        .await?;
        let mut sender = conn.sender;

        if is_websocket {
            return self
                .forward_upgrade(&mut sender, request, client_upgrade, request_id)
                .await;
        }

        let response =
            tokio::time::timeout(self.config.proxy_timeout(), sender.send_request(request))
                .await
                .map_err(|_| {
                    OagwError::new(
                        ErrorKind::RequestTimeout,
                        format!(
                            "upstream response timed out after {}s",
                            self.config.proxy_timeout_secs
                        ),
                    )
                })?
                .map_err(|err| {
                    OagwError::new(
                        ErrorKind::DownstreamError,
                        format!("upstream request failed: {err}"),
                    )
                })?;

        let status = response.status();
        let mut response_headers = response.headers().clone();
        header_ops::strip_hop_by_hop(&mut response_headers);
        let body = response.into_body();
        let streamed = IdleTimeoutBody::new(body, self.config.idle_timeout());
        Ok(ProxyOutcome::Pass {
            status,
            headers: response_headers,
            body: axum::body::Body::new(streamed),
            request_id: request_id.clone(),
        })
    }

    /// Perform a WebSocket upgrade and start the bidirectional splice.
    async fn forward_upgrade(
        &self,
        sender: &mut hyper::client::conn::http1::SendRequest<ProxyBody>,
        request: http::Request<ProxyBody>,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
        request_id: &Option<String>,
    ) -> Result<ProxyOutcome, OagwError> {
        let client_upgrade = client_upgrade.ok_or_else(|| {
            OagwError::new(
                ErrorKind::ProtocolError,
                "this connection does not support protocol upgrades",
            )
        })?;
        // hyper stores the client-side `OnUpgrade` in the *response* extensions
        // (`proto/h1/dispatch.rs` inserts it on the received message).
        let response = tokio::time::timeout(self.config.proxy_timeout(), sender.send_request(request))
            .await
            .map_err(|_| {
                OagwError::new(ErrorKind::RequestTimeout, "upstream WebSocket handshake timed out")
            })?
            .map_err(|err| {
                OagwError::new(
                    ErrorKind::ProtocolError,
                    format!("upstream WebSocket handshake failed: {err}"),
                )
            })?;
        let mut response = response;

        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(OagwError::new(
                ErrorKind::ProtocolError,
                format!(
                    "upstream did not accept the WebSocket upgrade ({})",
                    response.status()
                ),
            ));
        }
        let upstream_upgraded = hyper::upgrade::on(&mut response)
            .await
            .map_err(|err| {
                OagwError::new(
                    ErrorKind::StreamAborted,
                    format!("upstream WebSocket upgrade failed: {err}"),
                )
            })?;

        let (parts, _) = response.into_parts();
        let mut builder = axum::response::Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS);
        for (name, value) in parts.headers.iter() {
            builder = builder.header(name, value);
        }
        let response = builder
            .body(axum::body::Body::empty())
            .map_err(|err| {
                OagwError::new(ErrorKind::ProtocolError, format!("upgrade response failed: {err}"))
            })?;

        // Splice both directions once the client-side upgrade completes.
        tokio::spawn(async move {
            let client_io = match client_upgrade.await {
                Ok(io) => TokioIo(hyper_util::rt::TokioIo::new(io)),
                Err(err) => {
                    tracing::warn!(error = %err, "client WebSocket upgrade failed");
                    return;
                }
            };
            let mut upstream_io = TokioIo(hyper_util::rt::TokioIo::new(upstream_upgraded));
            let mut client_io = client_io;
            if let Err(err) = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
                tracing::debug!(error = %err, "WebSocket splice ended");
            }
        });

        Ok(ProxyOutcome::Upgrade {
            response,
            request_id: request_id.clone(),
        })
    }
}

/// Newtype so upgraded IOs can be passed to `tokio::io::copy_bidirectional`.
struct TokioIo<T>(hyper_util::rt::TokioIo<T>);

impl<T: hyper::rt::Read + Unpin> tokio::io::AsyncRead for TokioIo<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<T: hyper::rt::Write + Unpin> tokio::io::AsyncWrite for TokioIo<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Status of a proxy outcome, for structured logging.
fn outcome_status(outcome: &ProxyOutcome) -> u16 {
    match outcome {
        ProxyOutcome::Pass { status, .. } => status.as_u16(),
        ProxyOutcome::Upgrade { .. } => 101,
    }
}

/// `true` when the inbound request asks for a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    let upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("websocket"));
    connection && upgrade
}

/// Merge upstream and route header rules (route rules win).
#[must_use]
fn merge_headers(upstream: &Upstream, route: &Route) -> HeaderRules {
    let mut merged = upstream.headers.clone();
    if let Some(route_headers) = &route.headers {
        for (name, value) in &route_headers.request.set {
            merged.request.set.insert(name.clone(), value.clone());
        }
        for (name, value) in &route_headers.request.add {
            merged.request.add.insert(name.clone(), value.clone());
        }
        for name in &route_headers.request.remove {
            merged.request.remove.push(name.clone());
        }
        if route_headers.request.passthrough != PassthroughMode::None {
            merged.request.passthrough = route_headers.request.passthrough;
            merged.request.passthrough_allowlist =
                route_headers.request.passthrough_allowlist.clone();
        }
        for (name, value) in &route_headers.response.set {
            merged.response.set.insert(name.clone(), value.clone());
        }
        for (name, value) in &route_headers.response.add {
            merged.response.add.insert(name.clone(), value.clone());
        }
        for name in &route_headers.response.remove {
            merged.response.remove.push(name.clone());
        }
    }
    merged
}

/// Wrap an upstream body with a per-chunk idle timeout so a stalled SSE
/// stream cannot pin the connection forever.
struct IdleTimeoutBody {
    inner: Incoming,
    idle: std::time::Duration,
    deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl IdleTimeoutBody {
    fn new(inner: Incoming, idle: std::time::Duration) -> Self {
        Self {
            inner,
            idle,
            deadline: Box::pin(tokio::time::sleep(idle)),
        }
    }
}

impl ::http_body::Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = OagwError;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        use std::future::Future;
        match Pin::new(&mut self.inner).poll_frame(cx) {
            std::task::Poll::Ready(frame) => {
                let next = tokio::time::Instant::now() + self.idle;
                self.deadline.as_mut().reset(next);
                std::task::Poll::Ready(frame.map(|result| {
                    result.map_err(|err| {
                        OagwError::new(
                            ErrorKind::StreamAborted,
                            format!("upstream stream aborted: {err}"),
                        )
                    })
                }))
            }
            std::task::Poll::Pending => {
                if self.deadline.as_mut().poll(cx).is_ready() {
                    return std::task::Poll::Ready(Some(Err(OagwError::new(
                        ErrorKind::IdleTimeout,
                        format!("no upstream data for {}s", self.idle.as_secs()),
                    ))));
                }
                std::task::Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn detects_websocket_upgrade() {
        assert!(is_websocket_upgrade(&header_map(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
        ])));
        assert!(!is_websocket_upgrade(&header_map(&[
            ("connection", "keep-alive"),
            ("upgrade", "websocket"),
        ])));
        assert!(!is_websocket_upgrade(&header_map(&[
            ("connection", "upgrade"),
            ("upgrade", "h2c"),
        ])));
    }

    #[test]
    fn merges_route_headers_over_upstream() {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "a".to_owned(),
            enabled: true,
            server: crate::domain::model::ServerConfig { endpoints: vec![] },
            protocol: crate::domain::model::PROTOCOL_HTTP.to_owned(),
            tags: vec![],
            headers: HeaderRules {
                request: crate::domain::model::RequestHeaderRules {
                    passthrough: PassthroughMode::None,
                    ..Default::default()
                },
                response: crate::domain::model::ResponseHeaderRules::default(),
            },
            rate_limit: None,
            cors: None,
            auth: None,
            plugins: Default::default(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: upstream.tenant_id,
            upstream_id: upstream.id,
            tags: vec![],
            r#match: MatchRule::Http(crate::domain::model::HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: Default::default(),
            }),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            headers: Some(HeaderRules {
                request: crate::domain::model::RequestHeaderRules {
                    passthrough: PassthroughMode::All,
                    ..Default::default()
                },
                response: crate::domain::model::ResponseHeaderRules::default(),
            }),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let merged = merge_headers(&upstream, &route);
        assert_eq!(merged.request.passthrough, PassthroughMode::All);
    }
}
