//! `DataPlaneServiceImpl` — proxy orchestration.
//!
//! One pass per request, in the order fixed by `docs/DESIGN.md`
//! §"Proxy Request Flow":
//!
//! ```text
//! validate framing → resolve upstream+route → CORS → rate limits
//!   → select endpoint → build outbound request → auth plugin
//!   → guards → transform(on_request) → upstream exchange
//!   → guards/transform(on_response) → stream back
//! ```
//!
//! Bodies are never buffered. A plain response, an SSE stream and a
//! `101` upgrade all travel the same path; only the last one diverges, at the
//! point where the raw sockets are spliced together.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};

use crate::config::{BODY_LIMIT_BYTES, OagwConfig};
use crate::domain::cors;
use crate::domain::error::{DomainError, DomainResult, ErrorSource};
use crate::domain::gts_helpers;
use crate::domain::model::{RateScope, RateStrategy, Route};
use crate::domain::plugin::RequestContext;
use crate::domain::ratelimit::{RateKey, RateLimiterRegistry, rate_limit_headers};
use crate::domain::services::management::{ControlPlaneService, ResolvedTarget, outbound_path};
use crate::domain::services::proxy::{
    DataPlaneService, ERROR_SOURCE_HEADER, ProxyRequest, ProxyResponse, TARGET_HOST_HEADER,
};
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::PluginRegistries;

use super::chain::{self, CustomPluginLookup};
use super::connector::UpstreamConnector;
use super::endpoint::EndpointSelector;
use super::headers;

/// Maximum time an upgraded (WebSocket) relay may stay idle before the
/// gateway gives up on it. Deliberately generous: a WebSocket session is
/// long-lived by design, unlike the request budget.
const UPGRADE_HANDSHAKE_TIMEOUT_SECS: u64 = 30;

/// The shipped Data Plane.
pub struct DataPlaneServiceImpl {
    control_plane: Arc<dyn ControlPlaneService>,
    registries: Arc<PluginRegistries>,
    connector: Arc<UpstreamConnector>,
    selector: EndpointSelector,
    limiters: RateLimiterRegistry,
    metrics: Arc<OagwMetrics>,
    config: OagwConfig,
}

/// Adapter letting the chain resolver reach custom plugin definitions.
struct ControlPlaneLookup(Arc<dyn ControlPlaneService>);

#[async_trait]
impl CustomPluginLookup for ControlPlaneLookup {
    async fn lookup(&self, id: uuid::Uuid) -> DomainResult<Option<crate::domain::model::Plugin>> {
        self.0.resolve_custom_plugin(id).await
    }
}

impl DataPlaneServiceImpl {
    /// Wire the Data Plane.
    #[must_use]
    pub fn new(
        control_plane: Arc<dyn ControlPlaneService>,
        registries: Arc<PluginRegistries>,
        metrics: Arc<OagwMetrics>,
        config: OagwConfig,
    ) -> Self {
        let connector = Arc::new(UpstreamConnector::new(
            config.ssrf_policy.clone(),
            config.allow_http_upstream,
            config.connect_timeout(),
        ));
        Self {
            control_plane,
            registries,
            connector,
            selector: EndpointSelector::new(),
            limiters: RateLimiterRegistry::new(),
            metrics: Arc::clone(&metrics),
            config,
        }
    }

    /// Enforce every applicable rate limit, tightest counter first.
    fn enforce_rate_limits(
        &self,
        target: &ResolvedTarget,
        request: &ProxyRequest,
        route_label: &str,
    ) -> DomainResult<Vec<(String, String)>> {
        let mut response_headers = Vec::new();
        for limit in &target.rate_limits {
            let scope_id = match limit.config.scope {
                RateScope::Global => "global".to_owned(),
                RateScope::Tenant => request.security_context.subject_tenant_id().to_string(),
                RateScope::User => request.security_context.subject_id().to_string(),
                RateScope::Ip => request.client_ip.clone().unwrap_or_else(|| "unknown".to_owned()),
                RateScope::Route => target
                    .route
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |r| r.id.to_string()),
            };
            let key = RateKey::new(
                limit.resource_type,
                &limit.resource_id.to_string(),
                limit.config.scope,
                &scope_id,
            );
            let decision = self.limiters.check(&key, &limit.config);

            if limit.config.response_headers {
                for (name, value) in rate_limit_headers(&decision) {
                    response_headers.push((name.to_owned(), value));
                }
            }

            if !decision.allowed {
                self.metrics
                    .record_rate_limited(&target.upstream.alias, route_label);
                let detail = format!(
                    "Rate limit exceeded for upstream {}",
                    target.upstream.alias
                );
                return Err(DomainError::rate_limit_exceeded(detail)
                    .with_retry_after(decision.retry_after_secs)
                    .with_extension("host", target.upstream.alias.clone())
                    .with_extension(
                        "upstream_id",
                        gts_helpers::anonymous_id(
                            gts_helpers::UPSTREAM_TYPE,
                            target.upstream.id,
                        ),
                    ));
            }
            if decision.degraded {
                // `strategy: degrade` forwards with reduced functionality;
                // the marker lets the upstream (and our logs) see it.
                response_headers.push(("x-oagw-degraded".to_owned(), "true".to_owned()));
            }
            if limit.config.strategy == RateStrategy::Queue && decision.retry_after_secs > 0 {
                tracing::debug!(
                    target: "oagw.ratelimit",
                    key = key.as_str(),
                    "queue strategy: capacity available, proceeding"
                );
            }
        }
        Ok(response_headers)
    }

    /// Reject query parameters the route does not allow.
    fn validate_query(route: &Route, query: Option<&str>) -> DomainResult<Vec<(String, String)>> {
        let Some(raw) = query.filter(|q| !q.is_empty()) else {
            return Ok(Vec::new());
        };
        let allowlist = route
            .match_config
            .http
            .as_ref()
            .map(|http| http.query_allowlist.clone())
            .unwrap_or_default();

        let mut params = Vec::new();
        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            if !allowlist.iter().any(|allowed| allowed == key.as_ref()) {
                return Err(DomainError::validation(format!(
                    "query parameter '{key}' is not in the route's query_allowlist"
                )));
            }
            params.push((key.into_owned(), value.into_owned()));
        }
        Ok(params)
    }

    /// Whether the client asked for a protocol upgrade.
    fn requested_upgrade(request: &ProxyRequest) -> Option<String> {
        let connection = request
            .headers
            .get(http::header::CONNECTION)
            .and_then(|v| v.to_str().ok())?;
        if !connection
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        {
            return None;
        }
        request
            .headers
            .get(http::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    /// Re-attach the upgrade negotiation headers that the hop-by-hop strip
    /// removed. They are hop-by-hop *for a normal request*; for an upgrade
    /// they are the request.
    fn restore_upgrade_headers(outbound: &mut HeaderMap, inbound: &HeaderMap, protocol: &str) {
        if let Ok(value) = HeaderValue::from_str(protocol) {
            outbound.insert(http::header::UPGRADE, value);
        }
        outbound.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("Upgrade"),
        );
        for name in [
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
            "sec-websocket-extensions",
        ] {
            if let Some(value) = inbound.get(name)
                && let Ok(header) = HeaderName::try_from(name)
            {
                outbound.insert(header, value.clone());
            }
        }
    }

    /// Build the outbound URI in origin form.
    fn build_uri(path: &str, query: &[(String, String)]) -> DomainResult<Uri> {
        let mut target = String::from(path);
        if !query.is_empty() {
            let encoded: String = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .finish();
            target.push('?');
            target.push_str(&encoded);
        }
        Uri::try_from(target.as_str()).map_err(|err| {
            DomainError::validation(format!("could not build the upstream URI: {err}"))
        })
    }
}

/// The fields of one audit record, captured before the request is consumed.
///
/// `docs/DESIGN.md` §4.3 fixes the field set. Nothing here can carry request
/// or response content: no bodies, no query parameters and no headers beyond
/// the correlation id.
struct AuditRecord {
    request_id: String,
    tenant_id: String,
    principal_id: String,
    alias: String,
    method: String,
    path: String,
    request_size: u64,
}

impl AuditRecord {
    fn capture(request: &ProxyRequest) -> Self {
        Self {
            request_id: request
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("n/a")
                .to_owned(),
            tenant_id: request.security_context.subject_tenant_id().to_string(),
            principal_id: request.security_context.subject_id().to_string(),
            alias: request.alias.clone(),
            method: request.method.as_str().to_owned(),
            path: request.instance.clone(),
            request_size: request
                .headers
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }
    }

    fn emit(&self, outcome: &DomainResult<Response>, started: Instant) {
        let duration_ms = started.elapsed().as_millis();
        match outcome {
            Ok(response) => {
                let response_size: u64 = response
                    .headers()
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                tracing::info!(
                    target: "oagw.audit",
                    event = "proxy_request",
                    request_id = %self.request_id,
                    tenant_id = %self.tenant_id,
                    principal_id = %self.principal_id,
                    host = %self.alias,
                    path = %self.path,
                    method = %self.method,
                    status = response.status().as_u16(),
                    duration_ms,
                    request_size = self.request_size,
                    response_size,
                    error_type = tracing::field::Empty,
                    "proxy request completed"
                );
            }
            Err(err) => tracing::error!(
                target: "oagw.audit",
                event = "proxy_request",
                request_id = %self.request_id,
                tenant_id = %self.tenant_id,
                principal_id = %self.principal_id,
                host = %self.alias,
                path = %self.path,
                method = %self.method,
                status = err.status(),
                duration_ms,
                request_size = self.request_size,
                response_size = 0,
                error_type = %err.gts_type(),
                "proxy request failed"
            ),
        }
    }
}

#[async_trait]
impl DataPlaneService for DataPlaneServiceImpl {
    async fn execute(&self, request: ProxyRequest) -> DomainResult<ProxyResponse> {
        let started = Instant::now();
        let audit = AuditRecord::capture(&request);

        let result = self.dispatch(request, started).await;
        audit.emit(&result, started);
        result
    }
}

impl DataPlaneServiceImpl {
    /// Resolve, execute and instrument one request.
    async fn dispatch(
        &self,
        request: ProxyRequest,
        started: Instant,
    ) -> DomainResult<Response> {
        // 1. Framing checks happen before anything is buffered.
        headers::validate_framing(&request.headers, BODY_LIMIT_BYTES)?;

        // 2. Resolve the alias, the route, and the merged configuration.
        let target = self
            .control_plane
            .resolve_proxy_target(
                &request.security_context,
                &request.alias,
                request.method.as_str(),
                request.path_suffix.as_deref(),
            )
            .await?;

        let route_label = target
            .route
            .as_ref()
            .and_then(|r| r.match_config.http.as_ref())
            .map_or_else(|| "unmatched".to_owned(), |m| m.path.clone());
        let host_label = target.upstream.alias.clone();
        let method_label = request.method.as_str().to_owned();

        self.metrics.adjust_in_flight(&host_label, 1);
        let result = self
            .execute_inner(request, &target, &route_label, started)
            .await;
        self.metrics.adjust_in_flight(&host_label, -1);

        match &result {
            Ok(response) => {
                self.metrics.record_duration(
                    &host_label,
                    &route_label,
                    "total",
                    started.elapsed().as_secs_f64(),
                );
                self.metrics.record_request(
                    &host_label,
                    &method_label,
                    &route_label,
                    response.status().as_u16(),
                );
            }
            Err(err) => {
                self.metrics
                    .record_error(&host_label, &route_label, err.gts_type());
            }
        }
        result
    }
}

impl DataPlaneServiceImpl {
    #[allow(clippy::too_many_lines, reason = "the proxy pass is intentionally one readable sequence")]
    async fn execute_inner(
        &self,
        request: ProxyRequest,
        target: &ResolvedTarget,
        route_label: &str,
        started: Instant,
    ) -> DomainResult<Response> {
        // 3. CORS on an actual cross-origin request, before anything leaves
        //    the process (`ADR/0004-cors.md` §"Actual Request Handling").
        let origin = request
            .headers
            .get(http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut cors_headers: Vec<(String, String)> = Vec::new();
        if let (Some(policy), Some(origin)) = (target.cors.as_ref(), origin.as_ref())
            && policy.enabled
        {
            cors::enforce(policy, origin, request.method.as_str())?;
            cors_headers = cors::response_headers(policy, origin)
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
        }

        // 4. Rate limits.
        let mut extra_response_headers = self.enforce_rate_limits(target, &request, route_label)?;
        extra_response_headers.extend(cors_headers);

        // 5. A matched route is mandatory: the route is what says which
        //    upstream path and method are reachable at all.
        let route = target.route.as_ref().ok_or_else(|| {
            DomainError::not_found(format!(
                "no route on upstream '{}' matches {} /{}",
                target.upstream.alias,
                request.method,
                request
                    .path_suffix
                    .as_deref()
                    .unwrap_or_default()
                    .trim_start_matches('/')
            ))
            .with_extension(
                "upstream_id",
                gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, target.upstream.id),
            )
            .with_extension("host", target.upstream.alias.clone())
        })?;

        let path = outbound_path(route, request.path_suffix.as_deref())?;
        let query = Self::validate_query(route, request.query.as_deref())?;

        // 6. Endpoint selection.
        let target_host = request
            .headers
            .get(TARGET_HOST_HEADER)
            .and_then(|v| v.to_str().ok());
        let (endpoint, selection) = self.selector.select(&target.upstream, target_host)?;
        self.metrics.record_endpoint_selection(
            &gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, target.upstream.id),
            &endpoint.host,
            selection.as_str(),
        );

        let resolved = self
            .connector
            .resolve(endpoint.scheme, &endpoint.host, endpoint.effective_port())
            .await?;

        // 7. Build the outbound request and run the plugin chain over it.
        let header_bag = headers::build_outbound_headers(&request.headers, &target.headers)?;
        let mut plugin_ctx = RequestContext {
            security_context: request.security_context.clone(),
            config: serde_json::Map::new(),
            method: request.method.as_str().to_owned(),
            path: path.clone(),
            query,
            headers: header_bag,
            // Bodies are streamed, so plugins see headers and metadata only.
            body: None,
            upstream_alias: target.upstream.alias.clone(),
            upstream_host: endpoint.host.clone(),
        };

        let lookup = ControlPlaneLookup(Arc::clone(&self.control_plane));
        let entries = chain::resolve_chain(&self.registries, &lookup, &target.plugins).await?;

        // Auth first, then guards, then request transforms.
        if let Some(auth) = &target.auth
            && let Some(plugin_ref) = &auth.plugin_ref
        {
            plugin_ctx.config = auth.config.clone();
            let auth_started = Instant::now();
            chain::run_auth(&self.registries, plugin_ref, &mut plugin_ctx).await?;
            self.metrics.record_duration(
                &target.upstream.alias,
                route_label,
                "auth",
                auth_started.elapsed().as_secs_f64(),
            );
        }
        chain::run_request_phase(&entries, &mut plugin_ctx).await?;

        let mut outbound_headers = headers::bag_to_header_map(&plugin_ctx.headers)?;
        outbound_headers.insert(
            http::header::HOST,
            HeaderValue::from_str(&endpoint.authority()).map_err(|_| {
                DomainError::validation(format!("invalid upstream authority '{}'", endpoint.host))
            })?,
        );

        let upgrade_protocol = Self::requested_upgrade(&request);
        if let Some(protocol) = &upgrade_protocol {
            Self::restore_upgrade_headers(&mut outbound_headers, &request.headers, protocol);
        }

        let uri = Self::build_uri(&plugin_ctx.path, &plugin_ctx.query)?;
        let method = Method::from_bytes(plugin_ctx.method.as_bytes()).map_err(|_| {
            DomainError::validation(format!("invalid HTTP method '{}'", plugin_ctx.method))
        })?;

        let mut builder = http::Request::builder().method(method).uri(uri);
        if let Some(dest) = builder.headers_mut() {
            *dest = outbound_headers;
        }
        let outbound = builder.body(request.body).map_err(|err| {
            DomainError::validation(format!("could not build the upstream request: {err}"))
        })?;

        // 8. Upstream exchange.
        let upstream_started = Instant::now();
        let mut sender = self.connector.connect(&resolved).await?;
        sender.ready().await.map_err(|err| {
            DomainError::protocol_error(format!(
                "upstream connection to '{}' was not usable: {err}",
                endpoint.host
            ))
        })?;
        let budget = if upgrade_protocol.is_some() {
            std::time::Duration::from_secs(UPGRADE_HANDSHAKE_TIMEOUT_SECS)
                .max(self.config.proxy_timeout())
        } else {
            self.config.proxy_timeout()
        };
        let upstream_response = tokio::time::timeout(budget, sender.send_request(outbound))
            .await
            .map_err(|_| {
                DomainError::request_timeout(format!(
                    "upstream '{}' did not respond within {}s",
                    endpoint.host,
                    budget.as_secs()
                ))
                .with_extension("host", target.upstream.alias.clone())
                .with_extension("path", path.clone())
            })?
            .map_err(|err| {
                DomainError::downstream_error(format!(
                    "upstream '{}' failed: {err}",
                    endpoint.host
                ))
                .with_extension("host", target.upstream.alias.clone())
                .with_extension("path", path.clone())
            })?;
        self.metrics.record_duration(
            &target.upstream.alias,
            route_label,
            "upstream",
            upstream_started.elapsed().as_secs_f64(),
        );

        let status = upstream_response.status();

        // 9. A `101` splices the two connections and leaves HTTP behind.
        if status == StatusCode::SWITCHING_PROTOCOLS {
            return self.relay_upgrade(
                request.on_upgrade,
                upstream_response,
                extra_response_headers,
            );
        }

        let (upstream_parts, upstream_body) = upstream_response.into_parts();

        // 10. Response-side plugins see status and headers; the body stays a
        //     stream so SSE and large payloads flow through untouched.
        let mut response_ctx = crate::domain::plugin::ResponseContext {
            config: serde_json::Map::new(),
            status: status.as_u16(),
            headers: headers::header_map_to_bag(&upstream_parts.headers),
            body: None,
        };
        chain::run_response_phase(&entries, &mut response_ctx).await?;

        let mut final_headers =
            headers::apply_response_rules(upstream_parts.headers.clone(), &target.headers);
        // Anything a response transform added or replaced wins.
        for (name, value) in response_ctx.headers.iter() {
            if let (Ok(header), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value))
                && !final_headers.contains_key(&header)
            {
                final_headers.insert(header, value);
            }
        }
        for (name, value) in extra_response_headers {
            if let (Ok(header), Ok(value)) = (
                HeaderName::try_from(name.as_str()),
                HeaderValue::from_str(&value),
            ) {
                final_headers.insert(header, value);
            }
        }
        // `ADR/0007`: every response says where it came from. An upstream 5xx
        // is passed through unchanged — it is not a gateway error.
        final_headers.insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static("upstream"),
        );

        let mut response = Response::new(Body::new(upstream_body));
        *response.status_mut() = status;
        *response.headers_mut() = final_headers;

        // The audit record for this exchange is emitted once, by `execute`,
        // so that failures are logged with the same field set as successes.
        tracing::debug!(
            target: "oagw.proxy",
            upstream_path = %path,
            status = status.as_u16(),
            duration_ms = started.elapsed().as_millis(),
            "upstream exchange completed"
        );

        Ok(response)
    }

    /// Splice the client and upstream sockets after a `101`.
    fn relay_upgrade(
        &self,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
        mut upstream_response: http::Response<hyper::body::Incoming>,
        extra_headers: Vec<(String, String)>,
    ) -> DomainResult<Response> {
        let client_upgrade = client_upgrade.ok_or_else(|| {
            DomainError::protocol_error(
                "upstream switched protocols but the client did not request an upgrade",
            )
        })?;
        let upstream_upgrade = upstream_response
            .extensions_mut()
            .remove::<hyper::upgrade::OnUpgrade>()
            .ok_or_else(|| {
                DomainError::protocol_error("upstream 101 response carried no upgrade handle")
            })?;

        tokio::spawn(async move {
            let (client, upstream) = tokio::join!(client_upgrade, upstream_upgrade);
            let (client, upstream) = match (client, upstream) {
                (Ok(client), Ok(upstream)) => (client, upstream),
                (client, upstream) => {
                    tracing::warn!(
                        target: "oagw.proxy",
                        client_error = ?client.err(),
                        upstream_error = ?upstream.err(),
                        "protocol upgrade failed on one side; dropping the relay"
                    );
                    return;
                }
            };
            let mut client = hyper_util::rt::TokioIo::new(client);
            let mut upstream = hyper_util::rt::TokioIo::new(upstream);
            match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                Ok((to_upstream, to_client)) => tracing::debug!(
                    target: "oagw.proxy",
                    to_upstream,
                    to_client,
                    "upgraded relay closed"
                ),
                Err(err) => tracing::debug!(
                    target: "oagw.proxy",
                    error = %err,
                    "upgraded relay ended with an error"
                ),
            }
        });

        // Hand the upstream's own handshake response back to the client so
        // `Sec-WebSocket-Accept` and any negotiated subprotocol survive.
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        for (name, value) in upstream_response.headers() {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        for (name, value) in extra_headers {
            if let (Ok(header), Ok(value)) = (
                HeaderName::try_from(name.as_str()),
                HeaderValue::from_str(&value),
            ) {
                response.headers_mut().insert(header, value);
            }
        }
        response.headers_mut().insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static("upstream"),
        );
        Ok(response)
    }
}

/// Render a gateway error as an RFC 9457 problem response.
///
/// Kept next to the Data Plane so the `X-OAGW-Error-Source: gateway` marker
/// and the body are produced in one place.
#[must_use]
pub fn error_response(err: &DomainError, instance: &str) -> Response {
    crate::api::rest::error::problem_response(err, instance, ErrorSource::Gateway).into_response()
}
