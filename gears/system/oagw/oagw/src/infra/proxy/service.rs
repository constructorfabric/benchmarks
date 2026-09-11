//! Data Plane — proxy request orchestration.
//!
//! One pass per request: resolve configuration through the Control Plane,
//! validate the inbound shape, apply the rate limit, run the plugin chain
//! (auth → guards → transforms), pick an endpoint, and forward. The response
//! body is never buffered — plain HTTP, server-sent events and protocol
//! upgrades all leave here as streams.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use dashmap::DashMap;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use pingora_core::protocols::Stream as TransportStream;
use toolkit_security::SecurityContext;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts;
use crate::domain::merge::EffectiveConfig;
use crate::domain::model::{
    CorsConfig, Endpoint, PassthroughMode, PathSuffixMode, PluginBinding, RateLimitStrategy,
    Upstream,
};
use crate::domain::plugin::{
    AuthContext, ErrorContext, GuardDecision, PluginScope, ProxyRequest, ProxyResponseHead,
    RequestContext, ResponseContext,
};
use crate::domain::repo::PluginRepository;
use crate::domain::services::{ControlPlaneService, ProxyTarget, normalize_path};
use crate::infra::metrics::{OagwMetrics, SelectionMethod};
use crate::infra::plugin::PluginRegistries;
use crate::infra::proxy::connector::{BodyStream, UpstreamConnector};
use crate::infra::proxy::websocket::{self, UpgradeOutcome};
use crate::infra::rate_limit::{RateLimitSubject, RateLimiterRegistry};

/// Header that pins a request to one endpoint of a multi-endpoint pool.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Headers removed on both directions per RFC 9110 §7.6.1.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers OAGW consumes and never forwards, even under `passthrough: all`.
///
/// `authorization` and `cookie` carry the *caller's* platform credentials;
/// forwarding them to a third party would defeat credential isolation. An
/// operator who genuinely wants one forwarded names it in
/// `passthrough_allowlist`.
const NEVER_PASSTHROUGH: &[&str] = &[
    "host",
    "authorization",
    "cookie",
    TARGET_HOST_HEADER,
    "x-oagw-error-source",
    "content-length",
];

/// Entity headers that describe the body and therefore travel with it
/// regardless of the passthrough mode.
const ENTITY_HEADERS: &[&str] = &["content-type"];

/// A proxy request as it arrives from the transport layer.
pub struct IncomingRequest {
    pub method: Method,
    pub alias: String,
    /// Path beyond the alias, e.g. `/v1/chat/completions`. May be empty.
    pub path_suffix: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub client_ip: Option<String>,
    /// Request URI, used as the problem-details `instance`.
    pub instance: String,
    /// True when the caller offered an HTTP/1.1 upgrade.
    pub wants_upgrade: bool,
}

/// What the Data Plane produced.
pub enum ProxyOutcome {
    /// Ordinary (possibly streaming) response.
    Streamed {
        head: ProxyResponseHead,
        body: BodyStream,
    },
    /// Fully-read response — used when the body was consumed to make a
    /// decision, e.g. a refused upgrade.
    Buffered {
        head: ProxyResponseHead,
        body: Bytes,
    },
    /// The upstream switched protocols; the transport layer must now splice.
    Upgraded {
        headers: HeaderMap,
        stream: TransportStream,
        leftover: Bytes,
    },
}

/// Data Plane service.
pub struct DataPlaneService {
    control: Arc<ControlPlaneService>,
    connector: Arc<UpstreamConnector>,
    registries: Arc<PluginRegistries>,
    plugin_repo: Arc<dyn PluginRepository>,
    limiter: Arc<RateLimiterRegistry>,
    metrics: Arc<OagwMetrics>,
    config: OagwConfig,
    /// Round-robin cursor per upstream.
    cursors: DashMap<Uuid, Arc<AtomicUsize>>,
}

impl std::fmt::Debug for DataPlaneService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneService").finish_non_exhaustive()
    }
}

impl DataPlaneService {
    #[must_use]
    pub fn new(
        control: Arc<ControlPlaneService>,
        connector: Arc<UpstreamConnector>,
        registries: Arc<PluginRegistries>,
        plugin_repo: Arc<dyn PluginRepository>,
        limiter: Arc<RateLimiterRegistry>,
        metrics: Arc<OagwMetrics>,
        config: OagwConfig,
    ) -> Self {
        Self {
            control,
            connector,
            registries,
            plugin_repo,
            limiter,
            metrics,
            config,
            cursors: DashMap::new(),
        }
    }

    /// Execute one proxy request end to end.
    ///
    /// # Errors
    ///
    /// Any catalogued gateway error; the transport layer renders it as
    /// problem details with `X-OAGW-Error-Source: gateway`.
    pub async fn execute(
        &self,
        ctx: &SecurityContext,
        incoming: IncomingRequest,
    ) -> OagwResult<ProxyOutcome> {
        let started = Instant::now();
        let alias = alias::normalize(&incoming.alias);
        let result = self.execute_inner(ctx, &alias, incoming).await;
        let elapsed = started.elapsed().as_secs_f64();

        match &result {
            Ok(outcome) => {
                let status = match outcome {
                    ProxyOutcome::Streamed { head, .. } | ProxyOutcome::Buffered { head, .. } => {
                        head.status.as_u16()
                    }
                    ProxyOutcome::Upgraded { .. } => StatusCode::SWITCHING_PROTOCOLS.as_u16(),
                };
                self.metrics
                    .record_request(&alias, "", "", status, elapsed);
            }
            Err(err) => {
                self.metrics
                    .record_error(&alias, "", err.kind.gts_type());
            }
        }
        result
    }

    async fn execute_inner(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        incoming: IncomingRequest,
    ) -> OagwResult<ProxyOutcome> {
        if incoming.body.len() > self.config.max_request_body_bytes {
            return Err(OagwError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "request body of {} bytes exceeds the {} byte limit",
                    incoming.body.len(),
                    self.config.max_request_body_bytes
                ),
            ));
        }

        let target = self
            .control
            .resolve_proxy_target(ctx, alias, incoming.method.as_str(), &incoming.path_suffix)
            .await?;

        let http_match = target.route.http().ok_or_else(|| {
            OagwError::new(
                ErrorKind::ProtocolError,
                "gRPC proxying is not implemented in this build",
            )
        })?;

        // Path suffix handling. Longest-prefix matching already guarantees the
        // route path is a prefix, so the effective outbound path is the
        // inbound suffix; `disabled` simply refuses anything beyond the route.
        let route_path = normalize_path(&http_match.path);
        let inbound_path = normalize_path(&incoming.path_suffix);
        if http_match.path_suffix_mode == PathSuffixMode::Disabled && inbound_path != route_path {
            return Err(OagwError::validation(format!(
                "route '{route_path}' does not accept a path suffix"
            ))
            .with("path", inbound_path));
        }

        // Query allowlist: an unlisted parameter is a rejection, not a silent
        // drop, so a caller learns their request was not honoured verbatim.
        for (name, _) in &incoming.query {
            if !http_match.allows_query_param(name) {
                return Err(OagwError::validation(format!(
                    "query parameter '{name}' is not permitted on this route"
                ))
                .with("path", inbound_path.clone()));
            }
        }

        let effective = target.effective.clone();
        let origin = incoming
            .headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        if let Some(cors) = effective.cors.as_ref().filter(|cors| cors.enabled)
            && let Some(origin) = origin.as_deref()
        {
            enforce_cors(cors, origin, incoming.method.as_str())?;
        }

        let budget_headers = self
            .enforce_rate_limit(ctx, &target, &incoming, &inbound_path)
            .await?;

        let mut request = ProxyRequest {
            method: incoming.method.clone(),
            path: inbound_path.clone(),
            query: incoming.query.clone(),
            headers: self.build_outbound_headers(&incoming, &effective),
            body: incoming.body.clone(),
        };

        let scope = PluginScope {
            security_context: ctx,
            alias,
            upstream_id: target.upstream.id,
            route_id: Some(target.route.id),
        };

        self.run_auth(&effective, scope, &mut request).await?;
        let chain = self.resolve_chain(&effective.plugins);
        self.run_guards_request(&chain, scope, &mut request).await?;
        self.run_transform_request(&chain, scope, &mut request)
            .await?;

        let endpoint = self.select_endpoint(&target.upstream, &incoming)?;

        let outcome = if incoming.wants_upgrade {
            self.forward_upgrade(&endpoint, &request).await?
        } else {
            self.forward_http(&endpoint, &request).await?
        };

        let mut outcome = self
            .finish(outcome, &chain, scope, &effective, origin.as_deref())
            .await?;
        apply_rate_limit_headers(&mut outcome, &budget_headers);
        Ok(outcome)
    }

    // -- Response assembly -------------------------------------------------

    async fn finish(
        &self,
        outcome: ProxyOutcome,
        chain: &ResolvedChain,
        scope: PluginScope<'_>,
        effective: &EffectiveConfig,
        origin: Option<&str>,
    ) -> OagwResult<ProxyOutcome> {
        match outcome {
            ProxyOutcome::Upgraded { .. } => Ok(outcome),
            ProxyOutcome::Streamed { mut head, body } => {
                self.finish_head(&mut head, chain, scope, effective, origin)
                    .await?;
                Ok(ProxyOutcome::Streamed { head, body })
            }
            ProxyOutcome::Buffered { mut head, body } => {
                self.finish_head(&mut head, chain, scope, effective, origin)
                    .await?;
                Ok(ProxyOutcome::Buffered { head, body })
            }
        }
    }

    async fn finish_head(
        &self,
        head: &mut ProxyResponseHead,
        chain: &ResolvedChain,
        scope: PluginScope<'_>,
        effective: &EffectiveConfig,
        origin: Option<&str>,
    ) -> OagwResult<()> {
        strip_hop_by_hop(&mut head.headers);

        for (binding, plugin) in &chain.guards {
            let ctx = ResponseContext {
                scope,
                config: &binding.config,
                response: head,
            };
            match plugin.guard_response(&ctx).await? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } => {
                    return Err(guard_rejection(status, &error_code, &message));
                }
            }
        }

        for (binding, plugin) in &chain.transforms {
            let mut ctx = ResponseContext {
                scope,
                config: &binding.config,
                response: head,
            };
            plugin.transform_response(&mut ctx).await?;
        }

        apply_response_header_rules(&mut head.headers, effective);
        if let Some(cors) = effective.cors.as_ref().filter(|cors| cors.enabled)
            && let Some(origin) = origin
        {
            apply_cors_response_headers(&mut head.headers, cors, origin);
        }
        Ok(())
    }

    // -- Forwarding --------------------------------------------------------

    async fn forward_http(
        &self,
        endpoint: &Endpoint,
        request: &ProxyRequest,
    ) -> OagwResult<ProxyOutcome> {
        let response = self
            .connector
            .send(endpoint, request, self.config.proxy_timeout())
            .await?;
        Ok(ProxyOutcome::Streamed {
            head: response.head,
            body: response.body,
        })
    }

    async fn forward_upgrade(
        &self,
        endpoint: &Endpoint,
        request: &ProxyRequest,
    ) -> OagwResult<ProxyOutcome> {
        let outcome = websocket::perform_upgrade(
            &self.connector,
            endpoint,
            request,
            self.config.proxy_timeout(),
        )
        .await?;
        Ok(match outcome {
            UpgradeOutcome::Switching {
                headers,
                stream,
                leftover,
            } => ProxyOutcome::Upgraded {
                headers,
                stream,
                leftover,
            },
            UpgradeOutcome::Rejected { head, body } => ProxyOutcome::Buffered { head, body },
        })
    }

    // -- Endpoint selection ------------------------------------------------

    /// Apply the `X-OAGW-Target-Host` behaviour matrix (ADR 0001, Appendix A).
    fn select_endpoint(
        &self,
        upstream: &Upstream,
        incoming: &IncomingRequest,
    ) -> OagwResult<Endpoint> {
        let endpoints = &upstream.server.endpoints;
        let requested = incoming
            .headers
            .get(TARGET_HOST_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());

        if let Some(requested) = requested {
            if !is_plain_host(requested) {
                return Err(OagwError::new(
                    ErrorKind::InvalidTargetHost,
                    "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, \
                     or special characters)",
                )
                .with("upstream_id", upstream.gts_id())
                .with("invalid_value", requested.to_owned()));
            }
            let matched = upstream.endpoint_for_host(requested).ok_or_else(|| {
                OagwError::new(
                    ErrorKind::UnknownTargetHost,
                    format!(
                        "X-OAGW-Target-Host '{requested}' does not match any configured endpoint"
                    ),
                )
                .with("upstream_id", upstream.gts_id())
                .with("invalid_value", requested.to_owned())
                .with("valid_hosts", upstream.endpoint_hosts())
            })?;
            self.metrics.record_endpoint_selection(
                &upstream.gts_id(),
                &matched.host,
                SelectionMethod::ExplicitHeader,
            );
            return Ok(matched.clone());
        }

        let Some(first) = endpoints.first() else {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                "upstream has no endpoints",
            ));
        };

        if endpoints.len() == 1 {
            self.metrics.record_endpoint_selection(
                &upstream.gts_id(),
                &first.host,
                SelectionMethod::Default,
            );
            return Ok(first.clone());
        }

        // A multi-endpoint pool whose alias was derived from a common suffix
        // has no default member: the caller must say which host it means.
        if alias::compute_derived_alias(endpoints).is_some() {
            return Err(OagwError::new(
                ErrorKind::MissingTargetHost,
                format!(
                    "X-OAGW-Target-Host header required for multi-endpoint upstream with common \
                     suffix alias. Valid hosts: [{}]",
                    upstream.endpoint_hosts().join(", ")
                ),
            )
            .with("upstream_id", upstream.gts_id())
            .with("alias", upstream.alias.clone())
            .with("valid_hosts", upstream.endpoint_hosts()));
        }

        let cursor = self
            .cursors
            .entry(upstream.id)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        let index = cursor.fetch_add(1, Ordering::Relaxed) % endpoints.len();
        let chosen = &endpoints[index];
        self.metrics.record_endpoint_selection(
            &upstream.gts_id(),
            &chosen.host,
            SelectionMethod::RoundRobin,
        );
        Ok(chosen.clone())
    }

    // -- Rate limiting -----------------------------------------------------

    async fn enforce_rate_limit(
        &self,
        ctx: &SecurityContext,
        target: &ProxyTarget,
        incoming: &IncomingRequest,
        path: &str,
    ) -> OagwResult<Vec<(String, String)>> {
        let Some(config) = target.effective.rate_limit.as_ref() else {
            return Ok(Vec::new());
        };
        let subject = RateLimitSubject {
            tenant_id: ctx.subject_tenant_id(),
            subject_id: ctx.subject_id(),
            upstream_id: target.upstream.id,
            route_id: target.route.id,
        };
        let client_ip = incoming.client_ip.as_deref();

        let verdict = match config.strategy {
            RateLimitStrategy::Reject => self.limiter.check(config, &subject, client_ip),
            RateLimitStrategy::Queue => {
                self.limiter
                    .acquire_queued(config, &subject, client_ip, self.config.proxy_timeout())
                    .await
            }
            // Degrade: record the overage and let the request through with the
            // usage headers attached, rather than failing the caller.
            RateLimitStrategy::Degrade => {
                let verdict = self.limiter.check(config, &subject, client_ip);
                self.metrics.record_rate_limit(
                    &target.upstream.alias,
                    path,
                    false,
                    verdict.usage_ratio,
                );
                return Ok(rate_limit_headers(config, &verdict));
            }
        };

        self.metrics.record_rate_limit(
            &target.upstream.alias,
            path,
            !verdict.allowed,
            verdict.usage_ratio,
        );

        if verdict.allowed {
            return Ok(rate_limit_headers(config, &verdict));
        }

        warn!(
            target: "oagw.rate_limit",
            alias = %target.upstream.alias,
            path,
            limit = verdict.limit,
            "rate limit exceeded"
        );
        let mut error = OagwError::new(
            ErrorKind::RateLimitExceeded,
            format!("Rate limit exceeded for upstream {}", target.upstream.alias),
        )
        .with("host", target.upstream.alias.clone())
        .with("upstream_id", target.upstream.gts_id())
        .with("path", path.to_owned())
        .with("limit", verdict.limit)
        .with("remaining", verdict.remaining)
        .with_retry_after(verdict.retry_after_secs.max(1));
        // RFC 6585 / draft-ietf-httpapi-ratelimit-headers: a 429 always states
        // the budget it enforced, whatever `response_headers` says.
        for (name, value) in rate_limit_headers_forced(config, &verdict) {
            error = error.with_header(&name, value);
        }
        Err(error)
    }

    // -- Plugin chain ------------------------------------------------------

    async fn run_auth(
        &self,
        effective: &EffectiveConfig,
        scope: PluginScope<'_>,
        request: &mut ProxyRequest,
    ) -> OagwResult<()> {
        let Some(auth) = effective.auth.as_ref() else {
            return Ok(());
        };
        let Some(plugin_type) = auth.plugin_type.as_deref() else {
            return Ok(());
        };
        let Some(plugin) = self.registries.auth.get(plugin_type) else {
            return Err(OagwError::new(
                ErrorKind::PluginNotFound,
                format!("unknown auth plugin: {plugin_type}"),
            )
            .with("plugin_id", plugin_type.to_owned()));
        };

        let mut ctx = AuthContext {
            scope,
            config: &auth.config,
            headers: &mut request.headers,
            query: &mut request.query,
        };
        plugin.authenticate(&mut ctx).await?;
        Ok(())
    }

    /// Resolve every binding to a concrete plugin, in chain order.
    fn resolve_chain(&self, bindings: &[PluginBinding]) -> ResolvedChain {
        let mut chain = ResolvedChain::default();
        for binding in bindings {
            if let Some(plugin) = self.registries.guard.get(&binding.plugin_ref) {
                chain.guards.push((binding.clone(), plugin));
                continue;
            }
            if let Some(plugin) = self.registries.transform.get(&binding.plugin_ref) {
                chain.transforms.push((binding.clone(), plugin));
                continue;
            }
            if let Some(uuid) = binding.plugin_uuid {
                // Custom (Starlark) plugin definitions are stored and served
                // by the management API, but this build ships no interpreter,
                // so the binding is recorded and skipped rather than failing
                // every request through the upstream.
                self.plugin_repo.touch(uuid, now_epoch_secs());
                debug!(
                    target: "oagw.plugin",
                    plugin_ref = %binding.plugin_ref,
                    "custom plugin binding skipped: no interpreter in this build"
                );
                continue;
            }
            warn!(
                target: "oagw.plugin",
                plugin_ref = %binding.plugin_ref,
                "plugin binding could not be resolved and was skipped"
            );
        }
        chain
    }

    async fn run_guards_request(
        &self,
        chain: &ResolvedChain,
        scope: PluginScope<'_>,
        request: &mut ProxyRequest,
    ) -> OagwResult<()> {
        for (binding, plugin) in &chain.guards {
            let ctx = RequestContext {
                scope,
                config: &binding.config,
                request,
            };
            match plugin.guard_request(&ctx).await? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    error_code,
                    message,
                } => return Err(guard_rejection(status, &error_code, &message)),
            }
        }
        Ok(())
    }

    async fn run_transform_request(
        &self,
        chain: &ResolvedChain,
        scope: PluginScope<'_>,
        request: &mut ProxyRequest,
    ) -> OagwResult<()> {
        for (binding, plugin) in &chain.transforms {
            let mut ctx = RequestContext {
                scope,
                config: &binding.config,
                request,
            };
            plugin.transform_request(&mut ctx).await?;
        }
        Ok(())
    }

    /// Give transform plugins a chance to shape a gateway error.
    pub async fn transform_error(
        &self,
        bindings: &[PluginBinding],
        ctx: &SecurityContext,
        alias: &str,
        upstream_id: Uuid,
        error: &mut OagwError,
    ) {
        let chain = self.resolve_chain(bindings);
        let scope = PluginScope {
            security_context: ctx,
            alias,
            upstream_id,
            route_id: None,
        };
        for (binding, plugin) in &chain.transforms {
            let mut error_ctx = ErrorContext {
                scope,
                config: &binding.config,
                error,
            };
            if let Err(err) = plugin.transform_error(&mut error_ctx).await {
                warn!(target: "oagw.plugin", error = %err, "transform_error failed");
            }
        }
    }

    // -- Header assembly ---------------------------------------------------

    /// Build the outbound header set: passthrough policy first, then the
    /// configured remove/set/add rules.
    fn build_outbound_headers(
        &self,
        incoming: &IncomingRequest,
        effective: &EffectiveConfig,
    ) -> HeaderMap {
        let rules = &effective.headers.request;
        let mut out = HeaderMap::new();

        for (name, value) in &incoming.headers {
            let lower = name.as_str().to_ascii_lowercase();
            let allowlisted = rules
                .passthrough_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&lower));

            // Upgrade requests keep their handshake headers: they are
            // hop-by-hop by definition but are exactly what is being relayed.
            let is_upgrade_header = incoming.wants_upgrade
                && (lower == "connection"
                    || lower == "upgrade"
                    || lower.starts_with("sec-websocket-"));

            let keep = if is_upgrade_header {
                true
            } else if HOP_BY_HOP.contains(&lower.as_str()) {
                false
            } else if NEVER_PASSTHROUGH.contains(&lower.as_str()) {
                allowlisted && rules.passthrough == PassthroughMode::Allowlist
            } else if ENTITY_HEADERS.contains(&lower.as_str()) {
                true
            } else {
                match rules.passthrough {
                    PassthroughMode::None => false,
                    PassthroughMode::Allowlist => allowlisted,
                    PassthroughMode::All => true,
                }
            };

            if keep {
                out.append(name.clone(), value.clone());
            }
        }

        for name in &rules.remove {
            if let Ok(header_name) = HeaderName::try_from(name.to_ascii_lowercase()) {
                out.remove(&header_name);
            }
        }
        for (name, value) in &rules.set {
            if let (Ok(name), Ok(value)) = (
                HeaderName::try_from(name.to_ascii_lowercase()),
                HeaderValue::from_str(value),
            ) {
                out.insert(name, value);
            }
        }
        for (name, value) in &rules.add {
            if let (Ok(name), Ok(value)) = (
                HeaderName::try_from(name.to_ascii_lowercase()),
                HeaderValue::from_str(value),
            ) {
                out.append(name, value);
            }
        }
        out
    }
}

/// Standard rate-limit headers, emitted when the policy asks for them.
fn rate_limit_headers(
    config: &crate::domain::model::RateLimitConfig,
    verdict: &crate::infra::rate_limit::RateLimitVerdict,
) -> Vec<(String, String)> {
    if config.response_headers {
        rate_limit_headers_forced(config, verdict)
    } else {
        Vec::new()
    }
}

fn rate_limit_headers_forced(
    _config: &crate::domain::model::RateLimitConfig,
    verdict: &crate::infra::rate_limit::RateLimitVerdict,
) -> Vec<(String, String)> {
    let reset_at = now_epoch_secs().saturating_add(verdict.reset_after_secs);
    vec![
        ("x-ratelimit-limit".to_owned(), verdict.limit.to_string()),
        (
            "x-ratelimit-remaining".to_owned(),
            verdict.remaining.to_string(),
        ),
        ("x-ratelimit-reset".to_owned(), reset_at.to_string()),
    ]
}

/// Attach rate-limit headers to a successful outcome.
fn apply_rate_limit_headers(outcome: &mut ProxyOutcome, headers: &[(String, String)]) {
    if headers.is_empty() {
        return;
    }
    let target = match outcome {
        ProxyOutcome::Streamed { head, .. } | ProxyOutcome::Buffered { head, .. } => {
            &mut head.headers
        }
        // An upgraded connection stops being an HTTP message exchange; the
        // 101 head is the handshake, not a place for budget accounting.
        ProxyOutcome::Upgraded { .. } => return,
    };
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            target.insert(name, value);
        }
    }
}

/// Plugins resolved for one request, split by kind and kept in chain order.
#[derive(Default)]
struct ResolvedChain {
    guards: Vec<(PluginBinding, Arc<dyn crate::domain::plugin::GuardPlugin>)>,
    transforms: Vec<(PluginBinding, Arc<dyn crate::domain::plugin::TransformPlugin>)>,
}

/// Map a guard rejection onto the catalogued error whose status it carries.
fn guard_rejection(status: StatusCode, error_code: &str, message: &str) -> OagwError {
    let kind = match status {
        StatusCode::UNAUTHORIZED => ErrorKind::AuthenticationFailed,
        StatusCode::FORBIDDEN => ErrorKind::PermissionDenied,
        StatusCode::NOT_FOUND => ErrorKind::RouteNotFound,
        StatusCode::PAYLOAD_TOO_LARGE => ErrorKind::PayloadTooLarge,
        StatusCode::TOO_MANY_REQUESTS => ErrorKind::RateLimitExceeded,
        StatusCode::BAD_GATEWAY => ErrorKind::DownstreamError,
        StatusCode::GATEWAY_TIMEOUT => ErrorKind::RequestTimeout,
        _ => ErrorKind::ValidationError,
    };
    OagwError::new(kind, message.to_owned()).with("error_code", error_code.to_owned())
}

/// Reject a cross-origin request the CORS policy does not permit.
fn enforce_cors(cors: &CorsConfig, origin: &str, method: &str) -> OagwResult<()> {
    if !cors.origin_allowed(origin) {
        return Err(OagwError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("Origin '{origin}' not in allowed origins list"),
        ));
    }
    if !cors.method_allowed(method) {
        return Err(OagwError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("Method '{method}' not in allowed methods list"),
        ));
    }
    Ok(())
}

/// Attach the CORS members of an *actual* (non-preflight) response.
fn apply_cors_response_headers(headers: &mut HeaderMap, cors: &CorsConfig, origin: &str) {
    let allow_origin = if cors.allow_credentials || !cors.has_wildcard_origin() {
        origin.to_owned()
    } else {
        "*".to_owned()
    };
    if let Ok(value) = HeaderValue::from_str(&allow_origin) {
        headers.insert(
            HeaderName::from_static("access-control-allow-origin"),
            value,
        );
    }
    if cors.allow_credentials {
        headers.insert(
            HeaderName::from_static("access-control-allow-credentials"),
            HeaderValue::from_static("true"),
        );
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert(
            HeaderName::from_static("access-control-expose-headers"),
            value,
        );
    }
    // Always vary on Origin so a shared cache cannot serve one origin's
    // response to another.
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
}

/// Apply the configured response header rules.
fn apply_response_header_rules(headers: &mut HeaderMap, effective: &EffectiveConfig) {
    let rules = &effective.headers.response;
    for name in &rules.remove {
        if let Ok(header_name) = HeaderName::try_from(name.to_ascii_lowercase()) {
            headers.remove(&header_name);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.to_ascii_lowercase()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.to_ascii_lowercase()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

/// Drop hop-by-hop headers from a response before it is relayed.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        if let Ok(header_name) = HeaderName::try_from(*name) {
            headers.remove(&header_name);
        }
    }
}

/// A bare hostname or IP literal — no port, path, scheme or userinfo.
#[must_use]
pub fn is_plain_host(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    if alias::is_ip_literal(value) {
        return true;
    }
    value
        .split('.')
        .all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Log one completed proxy request in the audit format (ADR 0001, Appendix A).
pub fn audit_log(
    request_id: &str,
    tenant_id: Uuid,
    principal_id: Uuid,
    host: &str,
    path: &str,
    method: &str,
    status: u16,
    duration_ms: u128,
    error_type: Option<&str>,
) {
    info!(
        target: "oagw.audit",
        event = "proxy_request",
        request_id,
        tenant_id = %tenant_id,
        principal_id = %principal_id,
        host,
        path,
        method,
        status,
        duration_ms = duration_ms as u64,
        error_type = error_type.unwrap_or_default(),
        "proxy request completed"
    );
}

/// GTS identifier of the proxy permission, exported for the transport layer.
pub const PROXY_INVOKE_PERMISSION: &str = gts::PROXY_BASE;

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
