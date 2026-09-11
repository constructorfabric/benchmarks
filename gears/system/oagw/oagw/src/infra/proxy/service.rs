//! The proxy data plane (`DESIGN.md` § 3.2, `contracts/proxy-api.md` § 1).
//!
//! The service owns the ten-step pipeline: alias resolution → endpoint
//! selection → route match → rate limit → CORS → plugin chain → body limit →
//! upstream call → response transformation. Transport is `hyper-util` over
//! `hyper-rustls`; every decision before and after the socket is domain policy.

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method};
use hyper::body::Incoming;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
#[cfg(test)]
use crate::domain::model::Scheme;
use crate::domain::model::{Protocol, Route, Upstream};
use crate::domain::plugin::{PluginRuntime, RequestContext, ResponseContext};
use crate::domain::repo::ControlPlane;
use crate::domain::services::TenantChain;
use crate::domain::services::hierarchy::Ancestor;
use crate::domain::services::proxy as planning;
use crate::infra::cors as cors_policy;
use crate::infra::metrics::{CircuitState, ProxyMetrics};
use crate::infra::plugins::registry::PluginRegistry;
use crate::infra::proxy::client::{self, ProxyClient, UpstreamError};
use crate::infra::proxy::endpoint::EndpointSelector;
use crate::infra::proxy::headers as header_rules;
use crate::infra::ratelimit::{RateLimitDecision, RateLimiter, effective_limit};

/// Anything readable and writable in place.
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static {}
impl<T> Io for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static {}

/// The upstream response body a proxy request resolves to.
pub enum UpstreamBody {
    /// Fully read, for ordinary request/response exchanges.
    Buffered(Bytes),
    /// Streamed as it arrives, for SSE and long responses.
    Streamed(Incoming),
    /// Upgraded (WebSocket); the transport layer splices it with the client
    /// connection.
    Upgraded(Box<dyn Io>),
}

/// A pipeline rejection, with the bookkeeping the request had already earned.
#[derive(Debug)]
pub struct ProxyFailure {
    /// Why the pipeline rejected the request.
    pub error: DomainError,
    /// Rate-limit exposure of a request that passed step 5 and then failed.
    pub rate: Option<RateLimitDecision>,
}

impl From<DomainError> for ProxyFailure {
    fn from(error: DomainError) -> Self {
        Self { error, rate: None }
    }
}

impl ProxyFailure {
    /// Wraps an error raised after the rate limit was charged.
    #[must_use]
    pub fn charged(error: DomainError, rate: Option<RateLimitDecision>) -> Self {
        Self { error, rate }
    }
}

/// A resolved proxy response.
pub struct ProxyOutput {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Headers to return to the client, already transformed.
    pub headers: HeaderMap,
    /// Response body.
    pub body: UpstreamBody,
}

/// Everything the proxy needs about one inbound request.
pub struct ProxyInput {
    /// Upstream-bound method.
    pub method: Method,
    /// `{alias}[/{path_suffix}]`.
    pub rest: String,
    /// Raw query string.
    pub query: String,
    /// Inbound headers.
    pub headers: HeaderMap,
    /// Inbound body.
    pub body: Bytes,
    /// Caller tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject.
    pub subject_id: Uuid,
    /// Caller identity, for the plugin runtime.
    pub security: SecurityContext,
    /// Correlation identifier from the platform, when present.
    pub request_id: Option<String>,
}

/// The proxy data plane.
pub struct ProxyService {
    store: Arc<dyn ControlPlane>,
    chain: Arc<dyn TenantChain>,
    registry: Arc<PluginRegistry>,
    client: ProxyClient,
    config: crate::config::OagwConfig,
    limiter: RateLimiter,
    metrics: Arc<ProxyMetrics>,
    plugin_http: Arc<toolkit_http::HttpClient>,
    credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    selector: EndpointSelector,
}

impl std::fmt::Debug for ProxyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyService")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl ProxyService {
    /// Builds the data plane over a control plane.
    ///
    /// # Errors
    /// [`DomainError::Internal`] when the outbound client cannot be built.
    pub fn new(
        store: Arc<dyn ControlPlane>,
        chain: Arc<dyn TenantChain>,
        registry: Arc<PluginRegistry>,
        config: crate::config::OagwConfig,
        metrics: Arc<ProxyMetrics>,
    ) -> Result<Self, DomainError> {
        let client = client::build_client().map_err(|_| {
            DomainError::Internal("failed to build the outbound HTTP client".to_owned())
        })?;
        let plugin_http = Arc::new(
            toolkit_http::HttpClient::builder()
                .build()
                .map_err(|err| DomainError::Internal(format!("plugin http client: {err}")))?,
        );
        Ok(Self {
            store,
            chain,
            registry,
            client,
            config,
            selector: EndpointSelector::default(),
            limiter: RateLimiter::new(),
            metrics,
            plugin_http,
            credstore: None,
        })
    }

    /// Supplies the credential store the OAuth2 plugins resolve
    /// `secret_ref`s through (`ADR/0008`).
    #[must_use]
    pub fn with_credstore(
        mut self,
        credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>>,
    ) -> Self {
        self.credstore = credstore;
        self
    }

    /// The outbound client, exposed for tests.
    #[must_use]
    pub fn client(&self) -> &ProxyClient {
        &self.client
    }

    /// The metrics, exposed for the gear's health endpoint.
    #[must_use]
    pub fn metrics(&self) -> &Arc<ProxyMetrics> {
        &self.metrics
    }

    /// The hard request-body limit, applied at the transport boundary.
    #[must_use]
    pub fn config_max_body_bytes(&self) -> usize {
        self.config.max_body_bytes
    }

    /// Resolves the upstream for an alias, honouring the tenant chain.
    ///
    /// The walk stops at the closest match — shadowing — and the ancestor
    /// constraints that shadowing must not bypass are folded onto the result,
    /// so callers always see the configuration the request actually runs under
    /// ([`crate::domain::services::hierarchy::effective`]).
    ///
    /// # Errors
    /// [`DomainError::RouteNotFound`] when no upstream holds the alias,
    /// [`DomainError::LinkUnavailable`] when it is disabled.
    pub fn resolve_upstream(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
        // The caller's spelling of the alias is not the stored one: aliases are
        // normalized on submission, so the lookup is too (FR-004).
        let alias =
            crate::domain::alias::normalize_alias(alias).ok_or(DomainError::RouteNotFound)?;
        let chain = self.chain.chain(tenant_id);
        let mut selected: Option<Upstream> = None;
        let mut shadowed: Vec<Upstream> = Vec::new();
        for tenant in &chain {
            let Some(upstream) = self
                .store
                .upstreams()
                .list(*tenant)
                .into_iter()
                .find(|upstream| upstream.alias.as_deref() == Some(alias.as_str()))
            else {
                continue;
            };
            if selected.is_none() {
                selected = Some(upstream);
            } else {
                shadowed.push(upstream);
            }
        }
        let selected = selected.ok_or(DomainError::RouteNotFound)?;
        if !selected.is_enabled() {
            return Err(DomainError::LinkUnavailable);
        }
        // Shadowing selects the target only: the enforced ancestor constraints
        // survive it, so the caller sees the configuration it actually runs
        // under.
        let ancestors: Vec<_> = shadowed.iter().map(Ancestor::new).collect();
        Ok(crate::domain::services::hierarchy::effective(
            &selected, &ancestors,
        ))
    }

    /// Executes the pipeline for one request.
    ///
    /// # Errors
    /// Any [`DomainError`] the pipeline rejects with; the transport boundary
    /// renders it as RFC 9457 problem details with
    /// `X-OAGW-Error-Source: gateway`.
    #[allow(clippy::too_many_lines)]
    pub async fn handle(&self, input: ProxyInput) -> Result<ProxyOutput, ProxyFailure> {
        // 1-3: alias, endpoint and route. Everything before the rate limit is
        // rejected without rate-limit bookkeeping: the budget was never charged.
        let (alias, suffix) = planning::split_proxy_path(&input.rest);
        let upstream = self
            .resolve_upstream(input.tenant_id, &alias)
            .map_err(ProxyFailure::from)?;
        if upstream.protocol != Protocol::Http {
            return Err(ProxyFailure::from(DomainError::ProtocolError(
                upstream.protocol.as_str().to_owned(),
            )));
        }
        let target = self
            .selector
            .select(
                &upstream,
                input
                    .headers
                    .get(crate::infra::proxy::headers::TARGET_HOST_HEADER)
                    .and_then(|value| value.to_str().ok()),
            )
            .map_err(ProxyFailure::from)?;

        let upstream_id = upstream.id.unwrap_or_default();
        let routes = self
            .store
            .routes()
            .list_for_upstream(upstream.tenant_id, upstream_id);
        let matched = planning::match_http_route(&routes, input.method.as_str(), &suffix)
            .map_err(ProxyFailure::from)?;
        let route = matched.route.clone();
        let http_match = route
            .match_rule
            .http
            .clone()
            .ok_or(ProxyFailure::from(DomainError::RouteNotFound))?;
        planning::validate_query(&http_match, &input.query).map_err(ProxyFailure::from)?;

        // 5: rate limit. The route's limit, when declared, is stricter. From
        // here on the request has been charged, so every later rejection still
        // carries the exposure headers.
        let subject = self.subject_key(&input, &route);
        let mut rate_decision: Option<RateLimitDecision> = None;
        if let Some(limit) =
            effective_limit(upstream.rate_limit.as_ref(), route.rate_limit.as_ref())
        {
            let decision = self
                .limiter
                .check(
                    limit,
                    upstream_id,
                    route.id,
                    &subject,
                    std::time::Instant::now(),
                )
                .map_err(ProxyFailure::from)?;
            // Rate-limit state: the share of the bucket this request leaves
            // behind, against the capacity the limiter enforced.
            self.metrics.observe_rate_limit_usage(
                upstream_host(&upstream),
                &http_match.path,
                1.0 - f64::from(decision.remaining) / f64::from(capacity(limit)),
            );
            rate_decision = Some(decision);
        }
        let charged = |error| ProxyFailure::charged(error, rate_decision);

        // 6: CORS on the actual request.
        if let Some(cors) = upstream.cors.as_ref().filter(|cors| cors.enabled) {
            let origin = input
                .headers
                .get(http::header::ORIGIN)
                .and_then(|value| value.to_str().ok());
            cors_policy::validate_actual(cors, origin, input.method.as_str()).map_err(charged)?;
        }

        // 7: plugin chain — auth → guards → request transforms.
        let runtime = self.runtime(&input);
        let request_id = match input.request_id.clone() {
            Some(id) => id,
            None => Uuid::new_v4().to_string(),
        };
        let mut ctx = RequestContext {
            method: input.method.clone(),
            path: planning::outbound_path(&http_match.path, &matched.suffix),
            query: input.query.clone(),
            // Inbound headers pass through the upstream's request rules before
            // the plugin chain runs, so an auth plugin's injected headers are
            // never subject to `passthrough`.
            headers: header_rules::build_request_headers(&input.headers, upstream.headers.as_ref()),
            body: input.body.clone(),
            request_id: Some(request_id.clone()),
            config: serde_json::Value::Null,
            runtime: Arc::clone(&runtime),
        };
        self.run_request_plugins(&upstream, &route, &mut ctx)
            .await
            .map_err(charged)?;

        // 8: body limit.
        if ctx.body.len() > self.config.max_body_bytes {
            return Err(charged(DomainError::PayloadTooLarge(
                self.config.max_body_bytes,
            )));
        }

        // 9: upstream call.
        let wants_upgrade = planning::wants_upgrade(&input.headers);
        let response = self
            .call_upstream(
                &target,
                &ctx.path,
                &input.query,
                &ctx,
                &input.headers,
                wants_upgrade,
            )
            .await
            .map_err(charged)?;

        // 10: response transformation.
        self.finish(
            &upstream,
            &route,
            &input,
            &runtime,
            &request_id,
            rate_decision,
            response,
        )
        .await
        .map_err(charged)
    }

    /// Runs auth, guards and request transforms over the request context.
    async fn run_request_plugins(
        &self,
        upstream: &Upstream,
        route: &Route,
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let bindings = self.bindings(upstream, route);

        if let Some(auth) = upstream.auth.as_ref() {
            let plugin = self.registry.auth(&auth.kind)?;
            ctx.config = auth.config.clone();
            plugin.authenticate(ctx).await?;
        }

        for (id, config) in &bindings {
            if !self.is_guard(id) {
                continue;
            }
            let plugin = self.registry.guard(id)?;
            ctx.config = config.clone();
            match plugin.guard_request(ctx).await? {
                crate::domain::plugin::GuardDecision::Allow => {}
                crate::domain::plugin::GuardDecision::Reject(err) => return Err(err),
            }
        }

        for (id, config) in &bindings {
            if !self.is_transform(id) {
                continue;
            }
            let plugin = self.registry.transform(id)?;
            ctx.config = config.clone();
            plugin.transform_request(ctx).await?;
        }
        Ok(())
    }

    /// Applies the response-side plugin phases and shapes the output.
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        upstream: &Upstream,
        route: &Route,
        input: &ProxyInput,
        runtime: &Arc<PluginRuntime>,
        request_id: &str,
        rate_decision: Option<RateLimitDecision>,
        response: http::Response<Incoming>,
    ) -> Result<ProxyOutput, DomainError> {
        let status = response.status();
        let mut headers = header_rules::build_response_headers(
            response.headers(),
            upstream.headers.as_ref(),
            "upstream",
            Some(request_id),
        );
        if status == http::StatusCode::SWITCHING_PROTOCOLS {
            header_rules::apply_upgrade_response(&mut headers, response.headers());
        }
        // The rate-limit exposure headers ride along on every accepted request
        // too, so a caller can pace itself before the bucket runs dry.
        if let Some(decision) = rate_decision {
            header_rules::apply_rate_limit(&mut headers, &decision);
        }
        if let Some(cors) = upstream.cors.as_ref().filter(|cors| cors.enabled)
            && let Some(origin) = input
                .headers
                .get(http::header::ORIGIN)
                .and_then(|value| value.to_str().ok())
        {
            header_rules::apply_cors_response(&mut headers, cors, origin);
        }

        // Response-side guards and transforms run on the headers before the
        // body is streamed, so a policy rejection never leaks a partial body.
        let bindings = self.bindings(upstream, route);
        let mut response_ctx = ResponseContext {
            status: status.as_u16(),
            headers,
            request_id: Some(request_id.to_owned()),
            config: serde_json::Value::Null,
            runtime: Arc::clone(runtime),
        };
        for (id, config) in &bindings {
            if !self.is_guard(id) {
                continue;
            }
            let plugin = self.registry.guard(id)?;
            response_ctx.config = config.clone();
            match plugin.guard_response(&response_ctx).await? {
                crate::domain::plugin::GuardDecision::Allow => {}
                crate::domain::plugin::GuardDecision::Reject(err) => return Err(err),
            }
        }
        for (id, config) in &bindings {
            if !self.is_transform(id) {
                continue;
            }
            let plugin = self.registry.transform(id)?;
            response_ctx.config = config.clone();
            plugin.transform_response(&mut response_ctx).await?;
        }
        let headers = response_ctx.headers;

        if status == http::StatusCode::SWITCHING_PROTOCOLS {
            let upgraded = hyper::upgrade::on(response)
                .await
                .map_err(|_| DomainError::StreamAborted)?;
            self.metrics
                .set_circuit_breaker_state(upstream_host(upstream), CircuitState::Closed);
            self.metrics
                .record_upgrade(upstream_host(upstream), route_pattern(route));
            return Ok(ProxyOutput {
                status,
                headers,
                body: UpstreamBody::Upgraded(Box::new(hyper_util::rt::TokioIo::new(upgraded))),
            });
        }

        let streaming = streaming_content_type(&headers);
        self.metrics
            .set_circuit_breaker_state(upstream_host(upstream), CircuitState::Closed);
        self.metrics.record_request(
            upstream_host(upstream),
            input.method.as_str(),
            route_pattern(route),
            status.as_u16(),
        );
        if streaming {
            return Ok(ProxyOutput {
                status,
                headers,
                body: UpstreamBody::Streamed(response.into_body()),
            });
        }

        let body = client::read_body(response.into_body(), self.config.max_body_bytes)
            .await
            .map_err(map_upstream_error)?;
        Ok(ProxyOutput {
            status,
            headers,
            body: UpstreamBody::Buffered(body),
        })
    }

    /// Renders a gateway error for the error-transform phase.
    #[must_use]
    pub fn error_context(
        &self,
        input: &ProxyInput,
        error: DomainError,
    ) -> crate::domain::plugin::ErrorContext {
        crate::domain::plugin::ErrorContext {
            error: Some(error),
            headers: HeaderMap::new(),
            request_id: input.request_id.clone(),
            config: serde_json::Value::Null,
            runtime: self.runtime(input),
        }
    }

    /// Merged plugin bindings, upstream first, then route.
    fn bindings(&self, upstream: &Upstream, route: &Route) -> Vec<(String, serde_json::Value)> {
        let mut bindings: Vec<(String, serde_json::Value)> = Vec::new();
        for set in [upstream.plugins.as_ref(), route.plugins.as_ref()]
            .into_iter()
            .flatten()
        {
            for item in &set.items {
                bindings.push((item.reference().to_owned(), item.config().clone()));
            }
        }
        bindings
    }

    fn is_guard(&self, id: &str) -> bool {
        crate::domain::type_catalog::lookup(id)
            .is_some_and(|entry| entry.kind == crate::domain::model::PluginKind::Guard)
            || self.registry.guard(id).is_ok()
    }

    fn is_transform(&self, id: &str) -> bool {
        crate::domain::type_catalog::lookup(id)
            .is_some_and(|entry| entry.kind == crate::domain::model::PluginKind::Transform)
            || self.registry.transform(id).is_ok()
    }

    fn runtime(&self, input: &ProxyInput) -> Arc<PluginRuntime> {
        Arc::new(PluginRuntime {
            security: input.security.clone(),
            tenant_id: input.tenant_id,
            subject_id: input.subject_id,
            credstore: self.credstore.clone(),
            http: Arc::clone(&self.plugin_http),
            timeout: self.config.proxy_timeout(),
        })
    }

    /// The rate-limit bucket subject; the limiter ignores it for the shared
    /// global and per-route scopes.
    fn subject_key(&self, input: &ProxyInput, _route: &Route) -> String {
        format!("{}:{}", input.tenant_id, input.subject_id)
    }

    async fn call_upstream(
        &self,
        target: &crate::domain::model::Endpoint,
        path: &str,
        query: &str,
        ctx: &RequestContext,
        inbound: &HeaderMap,
        wants_upgrade: bool,
    ) -> Result<http::Response<Incoming>, DomainError> {
        let uri = format!(
            "{}://{}{}",
            target.scheme,
            target.authority(),
            path_and_query(path, query)
        );
        // `Host` is set exactly once: hyper's client would otherwise inject its
        // own from the URI, and a duplicate confuses strict upstreams.
        let mut builder = http::Request::builder()
            .method(ctx.method.clone())
            .uri(uri)
            .header(http::header::HOST, host_header(target))
            .header("x-request-id", ctx.request_id.clone().unwrap_or_default());
        for (name, value) in ctx.headers.iter() {
            if header_rules::is_hop_by_hop(name.as_str())
                && !(wants_upgrade && name == http::header::UPGRADE)
            {
                continue;
            }
            builder = builder.header(name, value.clone());
        }
        if wants_upgrade {
            // An upgrade is negotiated over headers the header policy does not
            // forward: `Upgrade` and `Connection` are hop-by-hop, and
            // `Sec-WebSocket-*` fall outside every passthrough mode. They are
            // copied verbatim for this one request, without duplicating a name
            // the policy already let through.
            let forwarded: Vec<&str> = ctx.headers.keys().map(http::HeaderName::as_str).collect();
            for (name, value) in inbound.iter() {
                let lower = name.as_str().to_ascii_lowercase();
                let upgrade_related = lower == "upgrade"
                    || lower == "connection"
                    || lower.starts_with("sec-websocket");
                if upgrade_related && !forwarded.contains(&lower.as_str()) {
                    builder = builder.header(name, value.clone());
                }
            }
        }
        let request = builder
            .body(ctx.body.clone())
            .map_err(|_| DomainError::Internal("invalid outbound request".to_owned()))?;
        client::send(&self.client, request, self.config.proxy_timeout())
            .await
            .map_err(map_upstream_error)
    }
}

fn map_upstream_error(error: UpstreamError) -> DomainError {
    match error {
        UpstreamError::Timeout => DomainError::RequestTimeout,
        UpstreamError::Connect | UpstreamError::Send => DomainError::LinkUnavailable,
        UpstreamError::Response => DomainError::DownstreamError,
    }
}

/// The bucket capacity a rate limit enforces.
fn capacity(limit: &crate::domain::model::RateLimit) -> u32 {
    limit.burst.capacity.unwrap_or(limit.sustained.rate).max(1)
}

/// The `host` label for the metrics: the upstream alias.
fn upstream_host(upstream: &Upstream) -> &str {
    upstream.alias.as_deref().unwrap_or_default()
}

/// The route's match pattern, for the metric labels.
fn route_pattern(route: &Route) -> &str {
    route
        .match_rule
        .http
        .as_ref()
        .map_or("", |rule| rule.path.as_str())
}

fn streaming_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

fn host_header(target: &crate::domain::model::Endpoint) -> String {
    target.authority()
}

fn path_and_query(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    }
}

/// Adds a query string to a path.
#[must_use]
pub fn with_query(path: &str, query: &str) -> String {
    path_and_query(path, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_faults_map_to_the_documented_statuses() {
        assert_eq!(map_upstream_error(UpstreamError::Timeout).status(), 504);
        assert_eq!(map_upstream_error(UpstreamError::Connect).status(), 503);
        assert_eq!(map_upstream_error(UpstreamError::Response).status(), 502);
        assert_eq!(map_upstream_error(UpstreamError::Send).status(), 503);
    }

    #[test]
    fn sse_is_detected_by_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            "text/event-stream".parse().unwrap(),
        );
        assert!(streaming_content_type(&headers));
        headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        assert!(!streaming_content_type(&headers));
    }

    #[test]
    fn the_host_header_is_the_endpoint_authority() {
        let target = crate::domain::model::Endpoint {
            scheme: Scheme::Https,
            host: "api.example.com".to_owned(),
            port: None,
        };
        assert_eq!(host_header(&target), "api.example.com:443");
    }

    #[test]
    fn the_query_is_appended_only_when_present() {
        assert_eq!(with_query("/v1", "a=b"), "/v1?a=b");
        assert_eq!(with_query("/v1", ""), "/v1");
    }
}
