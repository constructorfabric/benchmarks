//! The proxy pipeline (ADR 0002 "Execution Order").
//!
//! One function, [`DataPlaneService::proxy`], runs the whole request:
//!
//! ```text
//! resolve alias -> preflight -> resolve route -> resolve plugins (upstream
//!   chain + the route's own chain) -> guards(request) -> rate limit -> auth
//!   -> transform(on_request) -> build outbound request -> dial upstream
//!   -> response -> transform(on_response) / guards(response)
//!   -> header transform(response) -> client
//! ```
//!
//! Failures at any stage take the error path: `transform(on_error)` runs, then
//! the caller renders the [`DomainError`] as an RFC 9457 problem document
//! (`api::rest::error`), always tagged `X-OAGW-Error-Source`.
//!
//! Bodies are **streamed**: the downstream body is handed to the upstream as a
//! stream and the upstream body is handed back under an idle timeout, so an SSE
//! exchange relays chunk-by-chunk and a 100 MiB upload is never buffered.
//!
//! The transport lives in [`crate::infra::proxy::transport`], alias / route /
//! tenant resolution in [`crate::infra::proxy::resolve`]; this module is the
//! orchestration and the only place that knows the stage order.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response};
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, Plugin, PluginBinding, PluginChain, PluginKind, Upstream};
use crate::domain::plugin::{
    ErrorContext, GuardDecision, PluginConfig, RequestContext, ResponseContext, TransformPhase,
    merge_config, plugin_registry_key,
};
use crate::domain::services::ManagementService;
use crate::infra::plugin::cors;
use crate::infra::plugin::metrics::normalize_method;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::plugin::timeout::TimeoutConfig;
use crate::infra::proxy::circuit::CircuitBreakers;
use crate::infra::proxy::guards::{self, BodyPolicy, SsrfPolicy};
use crate::infra::proxy::headers as h;
use crate::infra::proxy::metrics::{DpMetrics, LabelSet, status_class};
use crate::infra::proxy::rate_limit::{RateLimiter, counter_key};
use crate::infra::proxy::resolve::{self, MergedUpstream, RouteMatchResult};
use crate::infra::proxy::runtime::PluginRuntime;
use crate::infra::proxy::transport::{ProxyTransport, TransportError};
use crate::infra::proxy::upgrades;

/// The metrics label used when no route matched.
const UNROUTED: &str = "unrouted";

/// A failed exchange, ready to be rendered as a problem document.
///
/// `headers`/`body` are whatever the `on_error` transform phase produced; the
/// caller folds them into the response after the standard problem mapping.
#[derive(Debug)]
pub struct PipelineFailure {
    /// The error the problem document describes.
    pub error: DomainError,
    /// Headers added by `transform(on_error)`.
    pub headers: HeaderMap,
    /// Body produced by `transform(on_error)`, if any.
    pub body: Option<Bytes>,
    /// The metrics label of the request, when a route had matched.
    pub route: String,
    /// Target host, when one had been resolved.
    pub host: Option<String>,
    /// Proxied path, when one is known.
    pub path: Option<String>,
    /// Trace correlation id, when `transform(on_error)` produced one.
    pub trace_id: Option<String>,
}

impl From<DomainError> for PipelineFailure {
    fn from(error: DomainError) -> Self {
        Self {
            error,
            headers: HeaderMap::new(),
            body: None,
            route: UNROUTED.to_owned(),
            host: None,
            path: None,
            trace_id: None,
        }
    }
}

impl PipelineFailure {
    /// Attach the target host to the failure (surfaced as a problem member).
    fn with_host(mut self, host: &str) -> Self {
        self.host = Some(host.to_owned());
        self
    }

    /// Attach the proxied path to the failure.
    fn with_path(mut self, path: &str) -> Self {
        self.path = Some(path.to_owned());
        self
    }

    /// Merge response headers into the failure (rate-limit budget headers on a
    /// 429, headers produced by an `on_error` transform, …).
    fn with_headers(mut self, extra: HeaderMap) -> Self {
        self.headers.extend(extra);
        self
    }
}

/// A plugin chain entry resolved to a registry key and merged configuration.
struct ResolvedPlugin {
    kind: PluginKind,
    /// The chain level that owns the binding now in force (`false` when only
    /// the upstream bound it).
    at_route_level: bool,
    key: String,
    position: usize,
    config: serde_json::Value,
}

impl ResolvedPlugin {
    /// The [`PluginConfig`] handed to a plugin invocation.
    fn plugin_config(&self) -> PluginConfig {
        PluginConfig {
            plugin_id: self.key.clone(),
            position: self.position,
            at_upstream_level: !self.at_route_level,
            config: self.config.clone(),
        }
    }
}

/// Everything the data plane needs to proxy a request.
///
/// Built once per process by `gear.rs`; every field is shared, immutable state
/// except the round-robin cursor and the per-upstream counters/breakers, which
/// are internally synchronised.
pub struct DataPlaneService {
    management: Arc<ManagementService>,
    transport: Arc<ProxyTransport>,
    registries: Arc<PluginRegistries>,
    runtime: Arc<PluginRuntime>,
    rate_limiter: Arc<RateLimiter>,
    circuits: Arc<CircuitBreakers>,
    metrics: Arc<DpMetrics>,
    ssrf: SsrfPolicy,
    tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
    round_robin: AtomicUsize,
}

impl std::fmt::Debug for DataPlaneService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneService")
            .field("transport", &self.transport)
            .field("registries", &self.registries)
            .field("runtime", &self.runtime)
            .field("ssrf", &self.ssrf)
            .field("has_tenant_resolver", &self.tenant_resolver.is_some())
            .finish_non_exhaustive()
    }
}

impl DataPlaneService {
    /// Assemble the data plane.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        management: Arc<ManagementService>,
        transport: Arc<ProxyTransport>,
        registries: Arc<PluginRegistries>,
        runtime: Arc<PluginRuntime>,
        rate_limiter: Arc<RateLimiter>,
        circuits: Arc<CircuitBreakers>,
        metrics: Arc<DpMetrics>,
        ssrf: SsrfPolicy,
        tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
    ) -> Self {
        Self {
            management,
            transport,
            registries,
            runtime,
            rate_limiter,
            circuits,
            metrics,
            ssrf,
            tenant_resolver,
            round_robin: AtomicUsize::new(0),
        }
    }

    /// The control plane this data plane reads configuration from.
    #[must_use]
    pub const fn management(&self) -> &Arc<ManagementService> {
        &self.management
    }

    /// The shared upstream transport.
    #[must_use]
    pub const fn transport(&self) -> &Arc<ProxyTransport> {
        &self.transport
    }

    /// The data-plane counters.
    #[must_use]
    pub const fn metrics(&self) -> &Arc<DpMetrics> {
        &self.metrics
    }

    /// The plugin registries.
    #[must_use]
    pub const fn registries(&self) -> &Arc<PluginRegistries> {
        &self.registries
    }

    /// The plugin runtime (secret source, token cache, transport).
    #[must_use]
    pub const fn runtime(&self) -> &Arc<PluginRuntime> {
        &self.runtime
    }

    /// Run the pipeline for one proxied request.
    ///
    /// `alias` is the `{alias}` path segment, `path_suffix` the `{*path}`
    /// segment (empty for `/oagw/v1/proxy/{alias}`), `security` the caller's
    /// context and `client_ip` the best-effort client address.
    ///
    /// # Errors
    /// [`PipelineFailure`] for every gateway error. An upstream response that is
    /// merely unsuccessful (a `500` from the origin) is *not* an error here.
    pub async fn proxy(
        &self,
        inbound: Request<Body>,
        alias: &str,
        path_suffix: &str,
        security: SecurityContext,
        client_ip: Option<String>,
    ) -> Result<Response<Body>, PipelineFailure> {
        let started = Instant::now();
        self.metrics.begin();
        let method = normalize_method(inbound.method().as_str());
        let outcome = self
            .run(inbound, alias, path_suffix, security, client_ip)
            .await;
        match outcome {
            Ok((response, route)) => {
                self.record(route, method, response.status(), "gateway", started);
                self.metrics.end();
                Ok(response)
            }
            Err(failure) => {
                self.metrics.end();
                let source = failure.error.source().as_str();
                let status = http::StatusCode::from_u16(failure.error.status())
                    .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
                self.record(failure.route.clone(), method, status, source, started);
                if source == "gateway" {
                    self.metrics.rejected();
                }
                Err(failure)
            }
        }
    }

    /// Record one completed exchange.
    fn record(
        &self,
        route: String,
        method: &'static str,
        status: http::StatusCode,
        source: &'static str,
        started: Instant,
    ) {
        self.metrics.record(LabelSet {
            status: status_class(status.as_u16()),
            route,
            method,
            error_source: source,
        });
        tracing::info!(
            target: "oagw::audit",
            status = status.as_u16(),
            duration_ms = started.elapsed().as_millis() as u64,
            event = crate::infra::plugin::logging::events::REQUEST_COMPLETED,
            "oagw request"
        );
    }

    /// The pipeline proper; returns the response plus its metrics label.
    #[allow(clippy::too_many_lines)]
    async fn run(
        &self,
        inbound: Request<Body>,
        alias: &str,
        path_suffix: &str,
        security: SecurityContext,
        client_ip_hint: Option<String>,
    ) -> Result<(Response<Body>, String), PipelineFailure> {
        let cfg = self.management.config().clone();
        let method = inbound.method().clone();
        let request_path = resolve::normalize_request_path(path_suffix);
        let raw_query = inbound.uri().query().unwrap_or_default().to_owned();
        let target_host = header_str(inbound.headers(), "x-oagw-target-host");
        let origin = header_str(inbound.headers(), cors::headers::ORIGIN);
        let trace_id = header_str(inbound.headers(), "x-oagw-trace-id")
            .or_else(|| header_str(inbound.headers(), "x-request-id"));
        let client_ip = client_ip_hint.or_else(|| client_ip_from(inbound.headers()));

        // -------------------------------------------------------- 1. preflight
        // Detected and answered *before* any alias resolution. ADR 0004
        // "Preflight Request Handling": a browser preflight carries no
        // credentials, so "no tenant context is available for upstream
        // resolution" and the handler answers it permissively — echoing the
        // requested origin, method and headers — without consulting the
        // upstream's CORS config at all. Origin and method enforcement is
        // deferred to the actual request, which *is* authenticated. Resolving
        // the alias first would make every preflight depend on a tenant
        // context that the platform's own auth middleware deliberately
        // withholds from preflights.
        if cors::is_preflight(
            method.as_str(),
            origin.as_deref(),
            header_str(inbound.headers(), cors::headers::REQUEST_METHOD).as_deref(),
        ) {
            let response = self.preflight(origin.as_deref(), inbound.headers())?;
            return Ok((response, UNROUTED.to_owned()));
        }

        // ------------------------------------------------------------ 2. alias
        let tenant_chain = self.tenant_chain(&security).await;
        let merged = resolve::resolve_upstream(&self.management, &tenant_chain, alias)
            .map_err(PipelineFailure::from)?;
        let upstream = merged.upstream.clone();
        let mut route_label = non_empty(&upstream.alias, UNROUTED);

        // ---------------------------------------------------------- 3. route
        // Matched *before* plugin resolution: the route contributes its own
        // plugin chain to the effective one.
        let matched = match self
            .match_route(
                &tenant_chain,
                &merged,
                method.as_str(),
                &request_path,
                &raw_query,
            )
            .await?
        {
            RouteMatchResult::Matched(matched) => matched,
            RouteMatchResult::MethodNotAllowed { allow } => {
                return Err(DomainError::MethodNotAllowed {
                    method: method.as_str().to_owned(),
                    allow,
                }
                .into());
            }
        };
        if let Some(path) = http_path_of(&matched.route) {
            route_label = path;
        }

        // ------------------------------------------------- 4. plugin resolution
        // The effective chain is the merged upstream-level chain *and* the
        // route's own chain, upstream first (DESIGN "Plugin System":
        // `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`, ADR 0002).
        let plugins = self.resolve_plugins(
            &tenant_chain,
            &merged.plugins,
            matched.route.plugins.as_ref(),
        )?;
        let timeouts = self.timeout_config(&plugins);
        let budget = timeouts.effective_request_timeout(cfg.proxy_timeout_secs);

        // --------------------------------------------- 5. request-side context
        let mut outbound = h::project_request_headers(inbound.headers(), &merged.headers.request);
        // An upgrade handshake *is* the connection/upgrade pair, so it is the
        // one case where hop-by-hop headers are kept (see `upgrades`).
        let wants_upgrade = upgrades::is_upgrade(inbound.headers());
        if wants_upgrade {
            // RFC 6455 §4.1: the handshake *is* its protocol headers, so they
            // are carried over whatever the passthrough posture is — without
            // them the relayed request is not a handshake at all.
            h::carry_handshake_headers(&mut outbound, inbound.headers());
        }
        h::strip_hop_by_hop(&mut outbound, wants_upgrade);

        // ------------------------------------------------------------ 6. guards
        self.enforce_core_guards(
            inbound.headers(),
            &method,
            &matched,
            &merged,
            origin.as_deref(),
            BodyPolicy::new(cfg.body_limit_bytes),
        )?;
        let mut ctx = request_context(
            method.as_str(),
            &matched.upstream_path,
            &raw_query,
            inbound.headers(),
            &security,
            Some(upstream.id),
            Some(matched.route.id),
            Some(upstream.alias.clone()),
            trace_id.as_deref(),
            PluginConfig::default(),
        );
        for plugin in &plugins {
            if plugin.kind != PluginKind::Guard {
                continue;
            }
            let Some(implementation) = self.registries.guard.get(&plugin.key) else {
                return Err(plugin_unavailable(&plugin.key).into());
            };
            ctx.config = plugin.plugin_config();
            match implementation.guard_request(&ctx).await {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject {
                    status,
                    error_code,
                    detail,
                }) => {
                    let error = reject_error(status, error_code, detail, &plugin.key);
                    return Err(self.failure(error, &plugins, &ctx).await);
                }
                Err(err) => return Err(PipelineFailure::from(DomainError::from(err))),
            }
        }

        // ------------------------------------------------------ 7. rate limit
        let rate_headers = self
            .enforce_rate_limit(&merged, &matched, &security, client_ip.as_deref(), &cfg)
            .await?;

        // ------------------------------------------------------------ 8. auth
        {
            let mut auth_ctx = request_context(
                method.as_str(),
                &matched.upstream_path,
                &raw_query,
                &outbound,
                &security,
                Some(upstream.id),
                Some(matched.route.id),
                Some(upstream.alias.clone()),
                trace_id.as_deref(),
                PluginConfig::default(),
            );
            auth_ctx.attributes = ctx.attributes.clone();
            if let Err(err) = self
                .enforce_auth(&upstream, &tenant_chain, &mut auth_ctx)
                .await
            {
                return Err(self.failure(err, &plugins, &ctx).await);
            }
            for (name, value) in auth_ctx.headers.iter() {
                outbound.insert(name.clone(), value.clone());
            }
            ctx.attributes = auth_ctx.attributes;
        }

        // ------------------------------------------ 9. transform(on_request)
        for plugin in &plugins {
            if plugin.kind != PluginKind::Transform {
                continue;
            }
            let Some(implementation) = self.registries.transform.get(&plugin.key) else {
                return Err(plugin_unavailable(&plugin.key).into());
            };
            if !implementation.phases().contains(&TransformPhase::OnRequest) {
                continue;
            }
            ctx.config = plugin.plugin_config();
            ctx.headers = outbound.clone();
            implementation
                .transform_request(&mut ctx)
                .await
                .map_err(DomainError::from)?;
            outbound = ctx.headers.clone();
        }
        // Plugins may have rewritten the path or the query.
        let upstream_path = ctx.path.clone();
        let upstream_query = ctx.query.clone();
        h::apply_request_rules(&mut outbound, &merged.headers.request);

        // -------------------------------------------- 10. build + dial upstream
        let endpoint =
            resolve::select_endpoint(&upstream, target_host.as_deref(), self.next_index())?;
        self.ssrf_check(&endpoint)?;
        let now = Instant::now();
        if !self.circuits.allows(upstream.id, now) {
            return Err(self
                .failure(self.circuits.error(upstream.id, now), &plugins, &ctx)
                .await);
        }
        let uri = ProxyTransport::build_uri(&endpoint, &upstream_path, &upstream_query).map_err(
            |err| {
                tracing::debug!(error = ?err, "upstream URI rejected");
                DomainError::from(err)
            },
        )?;
        self.ssrf_apply(&uri)?;
        h::set_host(&mut outbound, &endpoint.authority());

        let wants_upgrade = wants_upgrade
            || matches!(
                endpoint.scheme,
                crate::domain::model::Scheme::Wss | crate::domain::model::Scheme::Wt
            );

        let mut builder = Request::builder().method(method.clone()).uri(uri);
        for (name, value) in outbound.iter() {
            builder = builder.header(name.as_str(), value.clone());
        }
        let outbound = builder
            .body(Body::empty())
            .map_err(|err| DomainError::ProtocolError {
                detail: format!("unable to build the upstream request: {err}"),
            })
            .map_err(PipelineFailure::from)?;

        // A WebSocket handshake is relayed verbatim and then tunnelled: the
        // 101 comes straight from the upstream and the sockets are spliced.
        if wants_upgrade {
            let response =
                upgrades::tunnel(&self.transport, inbound, outbound, &endpoint, budget).await;
            return match response {
                Ok(response) => Ok((response, route_label)),
                Err(err) => {
                    self.circuits.record_failure(upstream.id, Instant::now());
                    Err(self
                        .failure(err, &plugins, &ctx)
                        .await
                        .with_host(&endpoint.authority())
                        .with_path(&upstream_path))
                }
            };
        }

        let outbound = outbound.map(|_empty| inbound.into_body());
        let response = match self.transport.send_within(outbound, budget).await {
            Ok(response) => response,
            Err(err) => {
                self.circuits.record_failure(upstream.id, Instant::now());
                self.metrics.transport_failure();
                let error = transport_error(err, &upstream, &endpoint, &upstream_path);
                return Err(self
                    .failure(error, &plugins, &ctx)
                    .await
                    .with_host(&endpoint.authority())
                    .with_path(&upstream_path));
            }
        };
        self.circuits.record_success(upstream.id);

        // --------------------------------------------------- 11. response phase
        let response = self
            .respond(
                response,
                &merged,
                &plugins,
                &upstream,
                &matched,
                &endpoint,
                &upstream_path,
                origin.as_deref(),
                trace_id.as_deref(),
                &rate_headers,
                &ctx,
            )
            .await?;
        Ok((response, route_label))
    }

    /// Guards that are core data-plane logic (not plugins), plus the CORS
    /// origin/method enforcement of ADR 0004.
    fn enforce_core_guards(
        &self,
        inbound: &HeaderMap,
        method: &Method,
        matched: &resolve::MatchedRoute,
        merged: &MergedUpstream,
        origin: Option<&str>,
        policy: BodyPolicy,
    ) -> Result<(), DomainError> {
        guards::check_body_guards(inbound, method.as_str(), policy)?;
        guards::check_query_guard(matched)?;
        if let Some(cors_config) = merged.cors.as_ref()
            && let Some(origin) = origin.filter(|origin| !origin.trim().is_empty())
        {
            if !cors::origin_allowed(cors_config, origin) {
                return Err(DomainError::CorsOriginNotAllowed {
                    detail: format!(
                        "origin `{origin}` is not listed in this upstream's allowed_origins"
                    ),
                });
            }
            if !cors::method_allowed(cors_config, method.as_str()) {
                return Err(DomainError::CorsMethodNotAllowed {
                    detail: format!(
                        "method {} is not listed in this upstream's allowed_methods",
                        method.as_str()
                    ),
                });
            }
        }
        Ok(())
    }

    /// The rate-limit stage; returns the headers the decision advertises.
    async fn enforce_rate_limit(
        &self,
        merged: &MergedUpstream,
        matched: &resolve::MatchedRoute,
        security: &SecurityContext,
        client_ip: Option<&str>,
        cfg: &crate::config::OagwConfig,
    ) -> Result<HeaderMap, PipelineFailure> {
        let Some(limit) = effective_rate_limit(merged, matched) else {
            return Ok(HeaderMap::new());
        };
        let key = counter_key(
            merged.upstream.id,
            Some(matched.route.id),
            limit.scope,
            security.subject_tenant_id(),
            security.subject_id(),
            client_ip,
        );
        let mut decision = self.rate_limiter.check(&key, &limit, Instant::now());
        if !decision.allowed && limit.strategy == crate::domain::model::RateStrategy::Queue {
            // `queue` waits for a token instead of rejecting, bounded by the
            // proxy budget so a saturated counter cannot pin the request.
            let deadline = Instant::now() + Duration::from_secs(cfg.proxy_timeout_secs.max(1));
            while !decision.allowed && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(25)).await;
                decision = self.rate_limiter.check(&key, &limit, Instant::now());
            }
        }
        let mut headers = rate_limit_headers(&decision);
        if !decision.allowed {
            if limit.strategy == crate::domain::model::RateStrategy::Degrade {
                headers.insert(
                    HeaderName::from_static("x-oagw-degraded"),
                    HeaderValue::from_static("true"),
                );
                return Ok(headers);
            }
            return Err(PipelineFailure::from(DomainError::RateLimitExceeded {
                retry_after_seconds: decision.retry_after_seconds.max(1),
            })
            // ADR 0003 "More Information": the 429 carries the `X-RateLimit-*`
            // headers alongside `Retry-After`, not only the accepted responses.
            .with_headers(headers));
        }
        Ok(headers)
    }

    /// Execute the bound auth plugin, if the upstream declares one.
    async fn enforce_auth(
        &self,
        upstream: &Upstream,
        tenant_chain: &[Uuid],
        ctx: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let Some(binding) = upstream.auth.as_ref() else {
            return Ok(());
        };
        let reference = binding.plugin.plugin_ref.trim();
        if reference.is_empty() {
            return Ok(());
        }
        // A bare UUID names a plugin resource; the implementation is registered
        // under that resource's GTS instance id, so expand before the lookup.
        let key = self.registry_key(tenant_chain, reference);
        let Some(auth) = self.registries.auth.get(&key) else {
            return Err(DomainError::PluginUnavailable {
                detail: format!("unknown auth plugin `{key}`"),
            });
        };
        let default_config = self.plugin_default_config(tenant_chain, reference);
        ctx.config = PluginConfig {
            plugin_id: key,
            position: 0,
            at_upstream_level: true,
            config: merge_config(default_config.as_ref(), binding.config.as_ref()),
        };
        auth.authenticate(ctx).await.map_err(DomainError::from)
    }

    /// Render the upstream response to the client (stage 11).
    #[allow(clippy::too_many_arguments)]
    async fn respond(
        &self,
        response: http::Response<hyper::body::Incoming>,
        merged: &MergedUpstream,
        plugins: &[ResolvedPlugin],
        upstream: &Upstream,
        matched: &resolve::MatchedRoute,
        endpoint: &Endpoint,
        upstream_path: &str,
        origin: Option<&str>,
        trace_id: Option<&str>,
        rate_headers: &HeaderMap,
        request_ctx: &RequestContext,
    ) -> Result<Response<Body>, PipelineFailure> {
        let status = response.status();
        let mut headers = response.headers().clone();
        h::strip_hop_by_hop(&mut headers, false);

        let mut response_ctx = ResponseContext {
            status: status.as_u16(),
            headers: headers.clone(),
            body: None,
            tenant_id: request_ctx.tenant_id,
            upstream_id: Some(upstream.id),
            route_id: Some(matched.route.id),
            trace_id: request_ctx.trace_id.clone(),
            config: PluginConfig::default(),
            attributes: request_ctx.attributes.clone(),
        };
        for plugin in plugins {
            if plugin.kind != PluginKind::Transform {
                continue;
            }
            let Some(implementation) = self.registries.transform.get(&plugin.key) else {
                continue;
            };
            if !implementation
                .phases()
                .contains(&TransformPhase::OnResponse)
            {
                continue;
            }
            response_ctx.config = plugin.plugin_config();
            if let Err(err) = implementation.transform_response(&mut response_ctx).await {
                tracing::warn!(plugin = %plugin.key, error = %err, "response transform failed");
            }
        }
        for plugin in plugins {
            if plugin.kind != PluginKind::Guard {
                continue;
            }
            let Some(implementation) = self.registries.guard.get(&plugin.key) else {
                continue;
            };
            response_ctx.config = plugin.plugin_config();
            if let Ok(GuardDecision::Reject {
                status,
                error_code,
                detail,
            }) = implementation.guard_response(&response_ctx).await
            {
                let error = reject_error(status, error_code, detail, &plugin.key);
                return Err(self
                    .failure(error, plugins, request_ctx)
                    .await
                    .with_path(upstream_path)
                    .with_host(&endpoint.authority()));
            }
        }
        headers = response_ctx.headers;
        for (name, value) in rate_headers.iter() {
            headers.insert(name.clone(), value.clone());
        }

        // CORS response headers for an allowed cross-origin exchange (ADR 0004).
        if let Some(cors_config) = merged.cors.as_ref()
            && let cors::CorsDecision::Allow(cors_headers) =
                cors::decide(cors_config, origin, false)
        {
            for (name, value) in cors_headers {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    headers.insert(name, value);
                }
            }
        }

        h::apply_response_rules(&mut headers, &merged.headers.response);
        if let Some(trace_id) = trace_id
            && let Ok(value) = HeaderValue::from_str(trace_id)
        {
            headers.insert(HeaderName::from_static("x-oagw-trace-id"), value);
        }

        let mut builder = Response::builder().status(status);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let idle = self.idle_budget(plugins);
        let response = builder
            .body(Body::new(crate::infra::proxy::transport::TimedBody::new(
                response.into_body(),
                idle,
            )))
            .map_err(|err| DomainError::ProtocolError {
                detail: format!("unable to build the downstream response: {err}"),
            })
            .map_err(PipelineFailure::from)?;
        Ok(response)
    }

    /// Run `transform(on_error)` and package the failure for the caller.
    async fn failure(
        &self,
        error: DomainError,
        plugins: &[ResolvedPlugin],
        request_ctx: &RequestContext,
    ) -> PipelineFailure {
        let mut context = ErrorContext {
            status: error.status(),
            error: error.clone(),
            headers: HeaderMap::new(),
            body: None,
            tenant_id: request_ctx.tenant_id,
            upstream_id: request_ctx.upstream_id,
            trace_id: request_ctx.trace_id.clone(),
            config: PluginConfig::default(),
            attributes: request_ctx.attributes.clone(),
        };
        for plugin in plugins {
            if plugin.kind != PluginKind::Transform {
                continue;
            }
            let Some(implementation) = self.registries.transform.get(&plugin.key) else {
                continue;
            };
            if !implementation.phases().contains(&TransformPhase::OnError) {
                continue;
            }
            context.config = plugin.plugin_config();
            if let Err(err) = implementation.transform_error(&mut context).await {
                tracing::warn!(plugin = %plugin.key, error = %err, "error transform failed");
            }
        }
        // The client's correlation id is echoed on every response, errors
        // included (DESIGN "Error Response Format"), exactly as stage 11 does
        // for a successful exchange.
        if let Some(trace_id) = request_ctx.trace_id.as_ref()
            && let Ok(value) = HeaderValue::from_str(trace_id)
        {
            context
                .headers
                .entry(HeaderName::from_static("x-oagw-trace-id"))
                .or_insert(value);
        }
        PipelineFailure {
            error,
            headers: context.headers,
            body: context.body,
            route: http_path_of_option(request_ctx.route_id).unwrap_or_else(|| UNROUTED.to_owned()),
            host: None,
            path: None,
            trace_id: context.trace_id,
        }
    }

    /// A preflight response (ADR 0004): 204, no upstream dial, no tenant work.
    #[allow(clippy::result_large_err)] // PipelineFailure is the pipeline's own envelope
    fn preflight(
        &self,
        origin: Option<&str>,
        inbound: &HeaderMap,
    ) -> Result<Response<Body>, PipelineFailure> {
        let Some(origin) = origin else {
            return Err(DomainError::invalid("preflight without an Origin header").into());
        };
        let request_method = header_str(inbound, cors::headers::REQUEST_METHOD);
        let request_headers = header_str(inbound, cors::headers::REQUEST_HEADERS);
        let mut builder = Response::builder().status(http::StatusCode::NO_CONTENT);
        // A preflight is answered locally, so the response is gateway-owned
        // (ADR 0007: the header is present on every response, not only errors).
        builder = builder.header(crate::api::rest::error::OAGW_ERROR_SOURCE_HEADER, "gateway");
        for (name, value) in cors::preflight_response(
            origin,
            request_method.as_deref(),
            request_headers.as_deref(),
        ) {
            builder = builder.header(name, value);
        }
        builder
            .body(Body::empty())
            .map_err(|err| DomainError::ProtocolError {
                detail: format!("unable to build the preflight response: {err}"),
            })
            .map_err(PipelineFailure::from)
    }

    /// The tenant chain for a request: `[self, parent, …, root]`.
    async fn tenant_chain(&self, security: &SecurityContext) -> Vec<Uuid> {
        let tenant = security.subject_tenant_id();
        if tenant.is_nil() {
            return vec![tenant];
        }
        let Some(resolver) = self.tenant_resolver.as_ref() else {
            return vec![tenant];
        };
        match resolver
            .get_ancestors(security, TenantId(tenant), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => resolve::chain_from(
                tenant,
                &response
                    .ancestors
                    .iter()
                    .map(|t| t.id.0)
                    .collect::<Vec<_>>(),
            ),
            Err(err) => {
                tracing::warn!(error = %err, "tenant ancestors unavailable; using the calling tenant only");
                vec![tenant]
            }
        }
    }

    /// Match the request against the routes of the tenant chain, descendant
    /// first, so a descendant's route shadows an ancestor's.
    async fn match_route(
        &self,
        tenant_chain: &[Uuid],
        merged: &MergedUpstream,
        method: &str,
        request_path: &str,
        query: &str,
    ) -> Result<resolve::RouteMatchResult, PipelineFailure> {
        let store = self.management.store();
        for tenant in tenant_chain {
            match resolve::match_route(
                store.as_ref(),
                *tenant,
                &merged.upstream,
                method,
                request_path,
                query,
            ) {
                Ok(result) => return Ok(result),
                // No match in this tenant: an ancestor may still own one.
                Err(DomainError::RouteNotFound { .. }) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(DomainError::RouteNotFound {
            detail: format!(
                "no route of upstream `{}` matches {method} {request_path}",
                merged.upstream.alias
            ),
        }
        .into())
    }

    /// Classify and merge the effective plugin chain: the merged upstream-level
    /// chain followed by the matched route's own chain (DESIGN "Plugin System"),
    /// with a route binding overriding the same reference at upstream level.
    ///
    /// # Errors
    /// [`DomainError::PluginUnavailable`] for a reference neither the built-in
    /// registries nor a plugin resource of the tenant chain resolves.
    fn resolve_plugins(
        &self,
        tenant_chain: &[Uuid],
        upstream: &[PluginBinding],
        route: Option<&PluginChain>,
    ) -> Result<Vec<ResolvedPlugin>, DomainError> {
        let bindings = resolve::merge_route_plugins(upstream, route);
        // A reference the route also binds is executing *for* the route, so it
        // is reported as route-level even when it sits in an upstream slot.
        let route_keys: BTreeSet<String> = route
            .map(|chain| {
                chain
                    .items
                    .iter()
                    .map(|binding| self.registry_key(tenant_chain, binding.plugin_ref()))
                    .collect()
            })
            .unwrap_or_default();
        let mut out: Vec<ResolvedPlugin> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (position, binding) in bindings.iter().enumerate() {
            let raw = plugin_registry_key(binding.plugin_ref());
            if raw.is_empty() {
                continue;
            }
            // A bare UUID names a plugin *resource*; the implementation it
            // dispatches to is registered under that resource's GTS instance
            // id, so the reference is expanded before the lookup.
            let resource = self.plugin_resource(tenant_chain, &raw);
            let key = resource
                .as_ref()
                .map(|plugin| plugin.gts_id())
                .unwrap_or_else(|| raw.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            let kind = resource
                .as_ref()
                .map(|plugin| plugin.plugin_type)
                .or_else(|| PluginKind::from_type_id(&key))
                .or_else(|| self.plugin_kind(tenant_chain, &raw))
                .ok_or_else(|| DomainError::PluginUnavailable {
                    detail: format!("`{raw}` is not an OAGW plugin identifier"),
                })?;
            let default = resource.as_ref().and_then(|plugin| plugin.config.clone());
            out.push(ResolvedPlugin {
                kind,
                at_route_level: route_keys.contains(&key) || route_keys.contains(&raw),
                key,
                position,
                config: merge_config(default.as_ref(), binding.config()),
            });
        }
        Ok(out)
    }

    /// The plugin *resource* a reference names, when it is UUID-backed.
    ///
    /// Named (`gts.…`) references are built-ins and have no resource. A bare
    /// UUID is looked up across the tenant chain, because the upstream schema
    /// binds custom plugins by UUID while the registry is keyed by GTS id.
    fn plugin_resource(&self, tenant_chain: &[Uuid], reference: &str) -> Option<Plugin> {
        if reference.starts_with("gts.") {
            return None;
        }
        let id = crate::domain::gts_helpers::parse_gts_instance_id(reference)?;
        tenant_chain
            .iter()
            .find_map(|tenant| self.management.get_plugin(*tenant, id).ok())
    }

    /// The registry key a plugin reference resolves to: named references keep
    /// their GTS id, UUID-backed ones expand to their resource's GTS instance
    /// id (falling back to the raw UUID when the resource is unknown).
    fn registry_key(&self, tenant_chain: &[Uuid], reference: &str) -> String {
        let raw = plugin_registry_key(reference);
        self.plugin_resource(tenant_chain, &raw)
            .map(|plugin| plugin.gts_id())
            .unwrap_or(raw)
    }

    /// The default configuration of a custom (UUID-backed) plugin resource.
    fn plugin_default_config(
        &self,
        tenant_chain: &[Uuid],
        reference: &str,
    ) -> Option<serde_json::Value> {
        if reference.starts_with("gts.") {
            return None;
        }
        let id = crate::domain::gts_helpers::parse_gts_instance_id(reference)?;
        tenant_chain.iter().find_map(|tenant| {
            self.management
                .get_plugin(*tenant, id)
                .ok()
                .and_then(|plugin| plugin.config)
        })
    }

    /// Kind of a plugin reference: named references carry their type in the GTS
    /// type id, UUID-backed ones in the plugin resource.
    fn plugin_kind(&self, tenant_chain: &[Uuid], reference: &str) -> Option<PluginKind> {
        if reference.starts_with("gts.") {
            return PluginKind::from_type_id(reference);
        }
        let id = crate::domain::gts_helpers::parse_gts_instance_id(reference)?;
        tenant_chain.iter().find_map(|tenant| {
            self.management
                .get_plugin(*tenant, id)
                .ok()
                .map(|plugin| plugin.plugin_type)
        })
    }

    /// The `timeout` catalog identifier's configuration, when bound.
    fn timeout_config(&self, plugins: &[ResolvedPlugin]) -> TimeoutConfig {
        let guard_id = crate::infra::plugin::timeout::TIMEOUT_GUARD_PLUGIN_ID;
        plugins
            .iter()
            .find(|plugin| plugin.key == guard_id)
            .map_or_else(TimeoutConfig::default, |plugin| {
                TimeoutConfig::from_config(&plugin.config)
            })
    }

    /// Idle budget for a streamed response body.
    fn idle_budget(&self, plugins: &[ResolvedPlugin]) -> Duration {
        Duration::from_secs(self.timeout_config(plugins).idle_timeout_secs.max(1))
    }

    /// Next round-robin index.
    fn next_index(&self) -> usize {
        self.round_robin.fetch_add(1, Ordering::Relaxed)
    }

    /// Dial-time SSRF screen for a resolved endpoint.
    fn ssrf_check(&self, endpoint: &Endpoint) -> Result<(), DomainError> {
        let host = crate::domain::model::upstream::normalize_host(&endpoint.host);
        if self.ssrf.allows(&host) {
            Ok(())
        } else {
            tracing::warn!(host = %host, "outbound dial refused by the SSRF policy");
            Err(self.ssrf.error(&host))
        }
    }

    /// The same policy, applied to a fully built URI (token endpoints included).
    fn ssrf_apply(&self, uri: &http::Uri) -> Result<(), DomainError> {
        let Some(host) = uri.host() else {
            return Err(DomainError::ProtocolError {
                detail: "the upstream URI has no host".to_owned(),
            });
        };
        let host = crate::domain::model::upstream::normalize_host(host);
        if self.ssrf.allows(&host) {
            Ok(())
        } else {
            Err(self.ssrf.error(&host))
        }
    }
}

// ------------------------------------------------------------------ helpers

/// The effective rate-limit configuration: the merged upstream limit and the
/// route's own limit, whichever is tighter (ADR 0003).
fn effective_rate_limit(
    merged: &MergedUpstream,
    matched: &resolve::MatchedRoute,
) -> Option<crate::domain::model::RateLimitConfig> {
    let upstream_limit = merged
        .rate_limit
        .clone()
        .map(|limit| (crate::domain::model::SharingMode::Inherit, limit));
    resolve::merge_rate_limits(upstream_limit.as_slice(), matched.route.rate_limit.as_ref())
}

/// `X-RateLimit-*` headers a rate-limit decision advertises.
fn rate_limit_headers(decision: &crate::infra::proxy::rate_limit::RateLimitDecision) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut insert = |name: &'static str, value: String| {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    };
    insert("x-ratelimit-limit", decision.limit.to_string());
    insert("x-ratelimit-remaining", decision.remaining.to_string());
    insert("x-ratelimit-reset", decision.reset_seconds.to_string());
    headers
}

/// The `RequestContext` handed to a plugin phase.
///
/// `body` is always `None`: the data plane streams bodies and never hands
/// buffered content to a plugin.
#[allow(clippy::too_many_arguments)]
fn request_context(
    method: &str,
    path: &str,
    query: &str,
    headers: &HeaderMap,
    security: &SecurityContext,
    upstream_id: Option<Uuid>,
    route_id: Option<Uuid>,
    alias: Option<String>,
    trace_id: Option<&str>,
    config: PluginConfig,
) -> RequestContext {
    RequestContext {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        headers: headers.clone(),
        body: None,
        downstream_headers: headers.clone(),
        security: security.clone(),
        tenant_id: security.subject_tenant_id(),
        upstream_id,
        route_id,
        alias,
        trace_id: trace_id.map(str::to_owned),
        config,
        attributes: Default::default(),
    }
}

/// The error for a guard rejection, using the DESIGN status/error-code table.
fn reject_error(
    status: u16,
    error_code: &'static str,
    detail: String,
    plugin: &str,
) -> DomainError {
    tracing::debug!(plugin = %plugin, error_code, status, "guard rejected the exchange");
    match status {
        401 => DomainError::AuthenticationFailed { detail },
        403 => DomainError::CorsOriginNotAllowed { detail },
        500..=599 => DomainError::ProtocolError { detail },
        _ => DomainError::invalid(detail),
    }
}

/// A plugin bound in configuration but not resolvable in this process.
fn plugin_unavailable(plugin: &str) -> DomainError {
    DomainError::PluginUnavailable {
        detail: format!("plugin `{plugin}` is not available in this process"),
    }
}

/// Read a single (lossy) header value.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Best-effort client address for per-IP rate-limit scopes.
fn client_ip_from(headers: &HeaderMap) -> Option<String> {
    let forwarded = header_str(headers, "x-forwarded-for")?;
    forwarded
        .split(',')
        .map(str::trim)
        .find(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| header_str(headers, "x-real-ip"))
}

/// `"" -> fallback`.
fn non_empty(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

/// `match.http.path` of a route, when it is an HTTP route with a path.
fn http_path_of(route: &crate::domain::model::Route) -> Option<String> {
    match &route.match_config {
        crate::domain::model::RouteMatch::Http(http) if !http.path.is_empty() => {
            Some(http.path.clone())
        }
        _ => None,
    }
}

/// The metrics label for a route id we could not resolve back to a path.
fn http_path_of_option(route_id: Option<Uuid>) -> Option<String> {
    route_id.map(|id| format!("route:{id}"))
}

/// Map a transport failure onto the domain error the client sees.
///
/// A failed dial (refused / reset connection, unresolved host, failed TLS
/// handshake, a connection dropped before a usable response) is a `502
/// DownstreamError` (DESIGN error table); the gateway's own dial policy, the
/// protocol layer and the budget keep their distinct mappings.
fn transport_error(
    err: TransportError,
    upstream: &Upstream,
    endpoint: &Endpoint,
    path: &str,
) -> DomainError {
    let host = endpoint.authority();
    match err {
        TransportError::Timeout { detail } => DomainError::RequestTimeout {
            detail,
            upstream_id: Some(upstream.id),
            host: Some(host),
            path: Some(path.to_owned()),
            retry_after_seconds: None,
        },
        TransportError::Connect { detail } => DomainError::DownstreamError {
            detail: format!("upstream `{host}` is unreachable: {detail}"),
            upstream_id: Some(upstream.id),
            host: Some(host),
            path: Some(path.to_owned()),
        },
        TransportError::PlaintextDisabled { host } => DomainError::LinkUnavailable {
            detail: format!(
                "upstream `{host}` uses a cleartext scheme and `allow_http_upstream` is disabled"
            ),
            retry_after_seconds: None,
        },
        TransportError::InvalidUri { detail } => DomainError::ProtocolError { detail },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::ErrorSource;
    use crate::domain::gts_helpers::problem;

    /// A preflight is answered before any alias resolution (ADR 0004), so it
    /// needs no tenant context: the platform's auth middleware hands preflights
    /// an *anonymous* security context, and resolving the alias first would turn
    /// every browser preflight into a 404.
    #[tokio::test]
    async fn a_preflight_is_answered_without_a_resolvable_alias() {
        let service = ManagementService::new(
            Arc::new(crate::infra::storage::MemoryStorage::new()),
            crate::config::OagwConfig::default(),
        );
        let plane = DataPlaneService::new(
            Arc::new(service),
            Arc::new(
                ProxyTransport::with_defaults(Duration::from_secs(2), true).expect("transport"),
            ),
            Arc::new(crate::infra::plugin::registry::PluginRegistries::with_builtins()),
            Arc::new(crate::infra::proxy::runtime::PluginRuntime::new(
                Arc::new(crate::infra::proxy::secrets::InMemorySecretSource::new())
                    as Arc<dyn crate::infra::proxy::secrets::SecretSource>,
                Duration::from_secs(300),
                10_000,
                None,
            )),
            Arc::new(crate::infra::proxy::rate_limit::RateLimiter::new(16)),
            Arc::new(crate::infra::proxy::circuit::CircuitBreakers::default()),
            crate::infra::proxy::metrics::DpMetrics::new(),
            crate::infra::proxy::guards::SsrfPolicy::permissive(),
            None,
        );

        let request = Request::builder()
            .method(http::Method::OPTIONS)
            .uri("/oagw/v1/proxy/never-registered/users")
            .header(cors::headers::ORIGIN, "https://app.example.com")
            .header(cors::headers::REQUEST_METHOD, "POST")
            .header(cors::headers::REQUEST_HEADERS, "content-type,authorization")
            .body(Body::empty())
            .expect("preflight request");

        // The alias does not exist and the security context is anonymous — the
        // state a real preflight arrives in — yet ADR 0004 still requires the
        // permissive 204.
        let response = plane
            .proxy(
                request,
                "never-registered",
                "/users",
                SecurityContext::anonymous(),
                None,
            )
            .await
            .expect("a preflight never resolves an upstream");
        assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(cors::headers::ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example.com"),
            "the preflight echoes the requested origin"
        );
    }

    /// A bare-UUID plugin binding expands to the plugin resource's GTS
    /// instance id, which is the key an implementation registers under.
    #[test]
    fn a_uuid_plugin_binding_expands_to_its_resource_gts_id() {
        use crate::domain::model::plugin::PluginSourceRecord;
        use crate::domain::model::{Plugin, PluginKind};
        use crate::domain::services::management::PluginDraft;

        let tenant = uuid::Uuid::new_v4();
        let service = ManagementService::new(
            Arc::new(crate::infra::storage::MemoryStorage::new()),
            crate::config::OagwConfig::default(),
        );
        let created = service
            .create_plugin(
                tenant,
                PluginDraft {
                    enabled: true,
                    name: "custom".to_owned(),
                    description: None,
                    plugin_type: PluginKind::Guard,
                    implementation: None,
                    sharing: crate::domain::model::SharingMode::Private,
                    tags: Vec::new(),
                    config: Some(serde_json::json!({"required_request_headers": "x-a"})),
                    config_schema: None,
                    phases: Vec::new(),
                    source: PluginSourceRecord::default(),
                },
            )
            .expect("plugin resource created");

        let plane = DataPlaneService::new(
            Arc::new(service),
            Arc::new(
                ProxyTransport::with_defaults(Duration::from_secs(2), true).expect("transport"),
            ),
            Arc::new(crate::infra::plugin::registry::PluginRegistries::with_builtins()),
            Arc::new(crate::infra::proxy::runtime::PluginRuntime::new(
                Arc::new(crate::infra::proxy::secrets::InMemorySecretSource::new())
                    as Arc<dyn crate::infra::proxy::secrets::SecretSource>,
                Duration::from_secs(300),
                10_000,
                None,
            )),
            Arc::new(crate::infra::proxy::rate_limit::RateLimiter::new(16)),
            Arc::new(crate::infra::proxy::circuit::CircuitBreakers::default()),
            crate::infra::proxy::metrics::DpMetrics::new(),
            crate::infra::proxy::guards::SsrfPolicy::permissive(),
            None,
        );

        // A bare UUID resolves to the resource's own GTS instance id.
        assert_eq!(
            plane.registry_key(&[tenant], created.id.to_string().as_str()),
            created.gts_id()
        );
        // A named (built-in) reference is passed through untouched.
        let builtin = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
        assert_eq!(plane.registry_key(&[tenant], builtin), builtin);
        // An unknown UUID is still addressable, so the error can name it.
        let unknown = uuid::Uuid::new_v4();
        assert_eq!(
            plane.registry_key(&[tenant], unknown.to_string().as_str()),
            unknown.to_string()
        );
        // Named references have no plugin resource.
        assert!(plane.plugin_resource(&[tenant], builtin).is_none());
        let _ = Plugin::default();
    }

    #[test]
    fn rate_limit_headers_are_advertised() {
        let decision = crate::infra::proxy::rate_limit::RateLimitDecision {
            allowed: true,
            limit: 10,
            remaining: 7,
            reset_seconds: 3,
            retry_after_seconds: 0,
            degraded: false,
        };
        let headers = rate_limit_headers(&decision);
        assert_eq!(headers.get("x-ratelimit-limit").unwrap(), "10");
        assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "7");
        assert_eq!(headers.get("x-ratelimit-reset").unwrap(), "3");
    }

    #[test]
    fn forwarded_for_picks_the_first_hop() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "10.0.0.1, 10.0.0.2".parse().unwrap());
        assert_eq!(client_ip_from(&headers).as_deref(), Some("10.0.0.1"));
        assert!(client_ip_from(&HeaderMap::new()).is_none());
    }

    #[test]
    fn route_label_falls_back() {
        assert_eq!(non_empty("", UNROUTED), UNROUTED);
        assert_eq!(non_empty("api.example.com", UNROUTED), "api.example.com");
    }

    #[test]
    fn transport_errors_carry_context() {
        let upstream = Upstream::default();
        let endpoint = Endpoint {
            scheme: crate::domain::model::Scheme::Https,
            host: "api.example.com".to_owned(),
            port: None,
        };
        let err = transport_error(
            TransportError::Timeout {
                detail: "late".to_owned(),
            },
            &upstream,
            &endpoint,
            "/v1",
        );
        assert_eq!(err.status(), 504);
        let err = transport_error(
            TransportError::Connect {
                detail: "refused".to_owned(),
            },
            &upstream,
            &endpoint,
            "/v1",
        );
        // A failed dial is a 502 DownstreamError (DESIGN error table).
        assert_eq!(err.status(), 502);
        assert_eq!(err.problem_type(), problem::DOWNSTREAM_ERROR);
        assert_eq!(err.source(), ErrorSource::Gateway);
        let problem = crate::api::rest::error::OagwProblem::from(err);
        assert_eq!(
            problem.host.as_deref(),
            Some("api.example.com"),
            "the host that could not be reached is named"
        );
        assert_eq!(problem.path.as_deref(), Some("/v1"));
    }

    /// A merged upstream whose upstream-level rate limit is `limit`.
    fn merged_with_rate_limit(
        limit: Option<crate::domain::model::RateLimitConfig>,
    ) -> MergedUpstream {
        let mut upstream = Upstream {
            alias: "api.vendor.com".to_owned(),
            ..Upstream::default()
        };
        upstream.rate_limit = limit;
        resolve::merge_upstream(upstream, &[])
    }

    /// A matched route whose route-level rate limit is `limit`.
    fn matched_with_rate_limit(
        limit: Option<crate::domain::model::RateLimitConfig>,
    ) -> resolve::MatchedRoute {
        let route = crate::domain::model::Route {
            rate_limit: limit,
            ..crate::domain::model::Route::default()
        };
        resolve::MatchedRoute {
            route,
            upstream_path: "/v1".to_owned(),
            upstream_query: String::new(),
        }
    }

    fn sustained(
        rate: u64,
        window: crate::domain::model::RateWindow,
    ) -> crate::domain::model::RateLimitConfig {
        crate::domain::model::RateLimitConfig {
            sustained: crate::domain::model::SustainedRate { rate, window },
            ..crate::domain::model::RateLimitConfig::default()
        }
    }

    #[test]
    fn a_route_limit_shadows_a_looser_upstream_limit() {
        // 1/second at the route, 100/second on the upstream: the tighter,
        // route-level block is the effective one (ADR 0003).
        let merged = merged_with_rate_limit(Some(sustained(
            100,
            crate::domain::model::RateWindow::Second,
        )));
        let matched =
            matched_with_rate_limit(Some(sustained(1, crate::domain::model::RateWindow::Second)));
        let effective = effective_rate_limit(&merged, &matched).expect("a limit applies");
        assert_eq!(effective.sustained.rate, 1);
        assert_eq!(
            effective.sustained.window,
            crate::domain::model::RateWindow::Second
        );
    }

    #[test]
    fn an_upstream_limit_applies_when_the_route_declares_none() {
        let merged =
            merged_with_rate_limit(Some(sustained(5, crate::domain::model::RateWindow::Minute)));
        let matched = matched_with_rate_limit(None);
        let effective = effective_rate_limit(&merged, &matched).expect("a limit applies");
        assert_eq!(effective.sustained.rate, 5);
        assert_eq!(
            effective.sustained.window,
            crate::domain::model::RateWindow::Minute
        );
    }

    #[test]
    fn an_unlimited_upstream_and_route_impose_no_limit() {
        let merged = merged_with_rate_limit(None);
        let matched = matched_with_rate_limit(None);
        assert!(effective_rate_limit(&merged, &matched).is_none());
    }

    #[test]
    fn the_tighter_of_two_limits_wins_regardless_of_level() {
        // The upstream limit is tighter even though the route declares one.
        let merged =
            merged_with_rate_limit(Some(sustained(1, crate::domain::model::RateWindow::Minute)));
        let matched = matched_with_rate_limit(Some(sustained(
            500,
            crate::domain::model::RateWindow::Minute,
        )));
        let effective = effective_rate_limit(&merged, &matched).expect("a limit applies");
        assert_eq!(effective.sustained.rate, 1);
    }
}
