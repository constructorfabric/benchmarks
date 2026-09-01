//! Data-plane (proxy) service — the request path between the proxy handler
//! and upstream services (ADR-0001, DESIGN §"3.5 Proxy Request Flow").
//!
//! Responsibilities for one proxied call:
//! 1. Resolve the upstream by alias (tenant chain, descendant first).
//! 2. Match a route (HTTP methods + longest path prefix + priority,
//!    enabled routes only).
//! 3. Select a target endpoint (`X-OAGW-Target-Host` / round-robin).
//! 4. SSRF check, effective rate limit, CORS actual-request check (the
//!    effective CORS/auth/plugin sets merge ancestor `enforce`/`inherit`
//!    policies with the selected upstream and route).
//! 5. Header pipeline (single stable `x-request-id` seeded at entry) +
//!    auth / guard / transform plugins.
//! 6. Body validation and bounded buffering.
//! 7. Forward via the toolkit-http client; stream the response back,
//!    aborting when the upstream stalls for longer than the proxy budget.
//!
//! WebSocket upgrades are tunneled on the raw upgraded connections: the hyper
//! server installs an `OnUpgrade` in the request extensions, the toolkit-http
//! client (hyper) installs one in the 101 response extensions, and
//! `tokio::io::copy_bidirectional` relays bytes between the two until either
//! side closes. The upgraded 101 response relays the upstream handshake
//! headers verbatim (hop-by-hop stripping would break the handshake).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use http::header::{
    CONNECTION, CONTENT_LENGTH, HOST, HeaderName, HeaderValue, ORIGIN, TRANSFER_ENCODING,
};
use http::{HeaderMap, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use toolkit_http::{HttpClient, HttpClientBuilder, HttpClientConfig, HttpError, RequestBuilder};
use toolkit_security::SecurityContext;
use url::Url;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::{derive_alias, normalize_host, validate_host};
use crate::domain::control::ControlPlaneService;
use crate::domain::error::{DomainError, ProxyContext, ProxyError};
use crate::domain::model::{
    AuthConfig, BurstCapacity, CorsConfig, Endpoint, EndpointScheme, MatchRule, PassthroughMode,
    PathSuffixMode, PluginBindingItem, RateAlgorithm, RateLimit, RateScope, RateStrategy,
    RateWindow, RequestHeaders, ResponseHeaders, Route, RouteRecord, SharingMode, SustainedRate,
    UpstreamRecord,
};
use crate::domain::plugin::{PluginError, RequestCtx, ResponseCtx};
use crate::domain::ratelimit::{RateDecision, RateLimiter, rate_scope_key};
use crate::gts;
use crate::infra::cors;
use crate::infra::plugins::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry, ensure_request_id,
};
use crate::infra::ssrf;

/// The `X-OAGW-Target-Host` routing header (ADR-0001).
const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// `X-OAGW-Error-Source` value for upstream passthrough responses (ADR-0007).
const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Hop-by-hop headers stripped from requests and responses (RFC 9110).
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

/// Entity headers always forwarded regardless of the passthrough policy.
const ALWAYS_FORWARD: &[&str] = &["content-type", "accept", "accept-language"];

/// WebSocket handshake headers forwarded verbatim on upgrade requests.
const WEBSOCKET_HANDSHAKE: &[&str] = &[
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
];

/// Response-body wrapper that enforces an idle timeout while the upstream
/// body is streamed back to the caller.
///
/// The per-frame budget is the configured proxy timeout: if the upstream
/// stalls between frames for longer than the budget, the stream is aborted
/// with a logged [`ProxyError::IdleTimeout`]. A response status cannot be
/// changed once headers were sent, so the abort surfaces to the caller as a
/// truncated stream rather than a late `504`.
struct IdleTimeoutBody {
    inner: toolkit_http::ResponseBody,
    timeout: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
    request_id: String,
    tenant_id: Uuid,
}

// Sound to move as a whole: every field is either already `Unpin` (the boxed
// body and strings) or a `Pin<Box<Sleep>>` whose pointee is only ever pinned
// in place via `as_mut`.
impl Unpin for IdleTimeoutBody {}

impl IdleTimeoutBody {
    fn new(
        inner: toolkit_http::ResponseBody,
        timeout: Duration,
        request_id: String,
        tenant_id: Uuid,
    ) -> Self {
        Self {
            inner,
            timeout,
            sleep: Box::pin(tokio::time::sleep(timeout)),
            request_id,
            tenant_id,
        }
    }
}

impl hyper::body::Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                // Each in-band frame restarts the idle budget.
                this.sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.timeout);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(item) => Poll::Ready(item),
            Poll::Pending => {
                if this.sleep.as_mut().poll(cx).is_ready() {
                    tracing::warn!(
                        event = "oagw.proxy.idle_timeout",
                        request_id = %this.request_id,
                        tenant_id = %this.tenant_id,
                        "upstream response stream idle for longer than the proxy budget; aborting"
                    );
                    Poll::Ready(Some(Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream response stream idle timeout",
                    )))))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// Request-scoped context threaded through the data-plane pipeline.
///
/// Bundling the per-request data (caller, error context, bindings, policy
/// effects and plugin chain) keeps the response-phase helpers small and
/// auditable (ref: item 15 — no `too_many_arguments` workarounds left).
struct ReqCtx<'a> {
    /// Caller security context.
    security: &'a SecurityContext,
    /// Calling tenant.
    tenant_id: Uuid,
    /// Proxy error-context extensions (path, upstream, host).
    context: &'a ProxyContext,
    /// Selected upstream's GTS instance id.
    upstream_id: Option<&'a str>,
    /// Rate-limit headers to annotate the response with.
    rate_headers: Option<(u64, u64)>,
    /// Effective CORS configuration (chain merge + route override).
    cors_cfg: Option<&'a CorsConfig>,
    /// Inbound `Origin` header (for CORS response enrichment).
    inbound_origin: Option<HeaderValue>,
    /// Selected upstream's response header rules.
    response_rules: Option<&'a ResponseHeaders>,
    /// Effective plugin chain (items with per-plugin configuration).
    plugins: &'a [PluginBindingItem],
}

/// Data-plane service backing the proxy handler.
pub struct DataPlaneService {
    control: Arc<ControlPlaneService>,
    http: HttpClient,
    limiter: Arc<RateLimiter>,
    auth: AuthPluginRegistry,
    guards: GuardPluginRegistry,
    transforms: TransformPluginRegistry,
    config: OagwConfig,
    /// Round-robin counter for pools without `X-OAGW-Target-Host`.
    rr: AtomicU64,
}

impl DataPlaneService {
    /// Build the data plane: control-plane handle, credential store (for auth
    /// plugins), token-endpoint HTTP config (for the `OAuth2` plugin) and the
    /// gear configuration.
    ///
    /// The rate limiter is shared with the control plane so management
    /// deletions can evict affected scope buckets.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] when the downstream HTTP client cannot be built.
    pub fn new(
        control: Arc<ControlPlaneService>,
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_http_config: Option<HttpClientConfig>,
        config: OagwConfig,
    ) -> Result<Self, HttpError> {
        let http = HttpClientBuilder::with_config(HttpClientConfig::proxy()).build()?;
        let limiter = Arc::new(RateLimiter::new());
        control.attach_rate_limiter(limiter.clone());
        Ok(Self {
            control,
            http,
            limiter,
            auth: AuthPluginRegistry::with_builtins(
                credstore,
                token_http_config,
                config.token_cache,
                config.allow_insecure_token_endpoint,
            ),
            guards: GuardPluginRegistry::with_builtins(),
            transforms: TransformPluginRegistry::with_builtins(),
            config,
            rr: AtomicU64::new(0),
        })
    }

    /// Execute one proxied request.
    ///
    /// Seeds a single stable `x-request-id` before the pipeline runs (kept
    /// when the caller supplied one, otherwise minted), emits the structured
    /// start/complete log pair and delegates the sequential pipeline to
    /// [`DataPlaneService::proxy_inner`].
    ///
    /// `alias` is the decoded path segment; `rest` is the raw (still
    /// percent-encoded) path suffix captured by the handler's wildcard, so no
    /// decode/re-encode round trip is needed.
    ///
    /// # Errors
    ///
    /// Returns a [`ProxyError`] carrying the upstream/host/path context.
    pub async fn proxy(
        &self,
        security: &SecurityContext,
        mut req: Request<axum::body::Body>,
        alias: String,
        rest: &str,
    ) -> Result<Response<axum::body::Body>, ProxyError> {
        let tenant_id = security.subject_tenant_id();
        let request_id = ensure_request_id(req.headers_mut()).map_err(|e| {
            ProxyError::internal(ProxyContext::default(), format!("request id: {e}"))
        })?;
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        let start = Instant::now();
        tracing::info!(
            event = "oagw.proxy.start",
            %tenant_id,
            %alias,
            method = %method,
            %path,
            request_id = ?request_id,
            "proxied request started"
        );
        let result = self
            .proxy_inner(security, req, alias, rest, &request_id)
            .await;
        Self::log_proxy_result(&result, &request_id, start);
        result
    }

    /// Emit the per-request result log line (completion or failure).
    fn log_proxy_result(
        result: &Result<Response<axum::body::Body>, ProxyError>,
        request_id: &http::HeaderValue,
        start: Instant,
    ) {
        match result {
            Ok(response) => {
                tracing::info!(
                    event = "oagw.proxy.complete",
                    request_id = ?request_id,
                    status = response.status().as_u16(),
                    duration_ms = start.elapsed().as_millis(),
                    "proxied request completed"
                );
            }
            Err(error) => {
                tracing::warn!(
                    event = "oagw.proxy.error",
                    request_id = ?request_id,
                    error_kind = proxy_error_kind(error),
                    duration_ms = start.elapsed().as_millis(),
                    "proxied request failed"
                );
            }
        }
    }

    /// The sequential request pipeline (resolve, match, select, validate,
    /// forward, relay). Kept in one place for auditability; the per-call state
    /// lives in [`ReqCtx`].
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn proxy_inner(
        &self,
        security: &SecurityContext,
        mut req: Request<axum::body::Body>,
        alias: String,
        rest: &str,
        request_id: &http::HeaderValue,
    ) -> Result<Response<axum::body::Body>, ProxyError> {
        let tenant_id = security.subject_tenant_id();
        let method = req.method().clone();
        let request_uri = req.uri().clone();
        let raw_path = rest.to_owned();
        let context = ProxyContext {
            path: if raw_path.is_empty() {
                None
            } else {
                Some(raw_path.clone())
            },
            ..ProxyContext::default()
        };

        // 1. Resolve the upstream by alias (tenant chain, descendant first).
        let Some(resolved) = self
            .control
            .resolve_alias(security, tenant_id, &alias)
            .await
            .map_err(|e| map_domain_to_proxy(&e, context.clone()))?
        else {
            return Err(ProxyError::RouteNotFound {
                context: ProxyContext::default(),
            }
            .with_context(context));
        };
        let selected = &resolved.selected;
        if !selected.entity.enabled {
            return Err(ProxyError::LinkUnavailable {
                context: ProxyContext::default(),
            }
            .with_context(with_entity_context(&context, selected)));
        }
        // gRPC proxying is Phase 3 (catalog only — no code path).
        if selected.entity.is_grpc() {
            return Err(ProxyError::GrpcNotImplemented {
                context: ProxyContext::default(),
            }
            .with_context(with_entity_context(&context, selected)));
        }

        // 2. Match a route (descendant priority + longest path prefix, then
        //    explicit priority; disabled routes are skipped).
        let chain_tenants: Vec<Uuid> = std::iter::once(selected.tenant_id)
            .chain(resolved.chain.iter().map(|r| r.tenant_id))
            .collect();
        let routes = self
            .control
            .repo()
            .list_routes_for_tenants(&chain_tenants)
            .await
            .map_err(|e| ProxyError::internal(context.clone(), format!("route lookup: {e}")))?;
        let selected_id = selected
            .entity
            .id
            .as_deref()
            .and_then(gts::parse_resource_id);
        let request_path = normalize_request_path(&raw_path);
        let Some(route) = select_route(
            &routes,
            &chain_tenants,
            selected_id,
            method.as_str(),
            &request_path,
        ) else {
            return Err(ProxyError::RouteNotFound {
                context: ProxyContext::default(),
            }
            .with_context(with_entity_context(&context, selected)));
        };
        let route_entity = route.entity.clone();
        let route_bare_id = route_entity
            .id
            .as_deref()
            .and_then(gts::parse_resource_id)
            .map(|u| u.to_string());

        // 3. Select the target endpoint (ADR-0001 matrix).
        let target_context = with_entity_context(&context, selected);
        let target = self
            .select_endpoint(selected, req.headers())
            .map_err(|e| e.with_context(target_context))?;

        // 4. SSRF.
        let verdict = ssrf::check_host(self.config.ssrf_policy, &target.host).await;
        if !matches!(verdict, ssrf::SsrfVerdict::Allowed) {
            return Err(ProxyError::ProtocolError {
                context: context.clone(),
                detail: format!(
                    "target host {:?} is not allowed by the SSRF policy",
                    target.host
                ),
            });
        }

        // 5. Effective policies across the chain (enforced ancestors win for
        //    auth; CORS and plugins merge ancestor `enforce`/`inherit` with
        //    the selected upstream and route).
        let effective_auth = effective_auth_binding(&resolved.chain, selected);
        let effective_cors =
            effective_cors_config(&resolved.chain, selected, route_entity.cors.as_ref());
        let plugins = effective_plugins(&resolved.chain, selected, &route_entity);

        let ip = client_ip(&req, self.config.trust_x_forwarded_for);

        // Effective rate limit (route + selected + enforced ancestors).
        let mut policies: Vec<RateLimit> = Vec::new();
        let mut ancestor_enforce_scope: Option<RateScope> = None;
        if let Some(rl) = &route_entity.rate_limit {
            policies.push(rl.clone());
        }
        if let Some(rl) = &selected.entity.rate_limit {
            policies.push(rl.clone());
        }
        for ancestor in resolved
            .chain
            .iter()
            .filter(|r| r.tenant_id != selected.tenant_id)
        {
            if let Some(rl) = &ancestor.entity.rate_limit
                && rl.sharing == SharingMode::Enforce
            {
                // An enforced ancestor defines the strictest bucket; its scope
                // determines the counter key.
                ancestor_enforce_scope = Some(rl.scope);
                policies.push(rl.clone());
            }
        }
        let mut rate_headers: Option<(u64, u64)> = None;
        if let Some(policy) = merge_rate_limits(&policies) {
            let scope = ancestor_enforce_scope
                .unwrap_or_else(|| policies.first().map_or(RateScope::Tenant, |p| p.scope));
            let key = rate_key(scope, security, tenant_id, &ip, route_bare_id.as_deref());
            match self.limiter.check(&key, &policy) {
                RateDecision::Allow { limit, remaining } => {
                    rate_headers = Some((limit, remaining));
                }
                RateDecision::Limited {
                    retry_after_secs,
                    limit,
                } => {
                    let now = duration_since_epoch().as_secs();
                    return Err(ProxyError::RateLimitExceeded {
                        context: ProxyContext::default(),
                        retry_after: Duration::from_secs(retry_after_secs),
                        limit,
                        remaining: 0,
                        reset: now.saturating_add(retry_after_secs),
                    }
                    .with_context(context.clone()));
                }
            }
        }

        // 6. CORS actual-request check (effective config).
        let inbound_origin = req.headers().get(ORIGIN).cloned();
        if let Some(cors_cfg) = &effective_cors {
            let dummy = bare_request(&req);
            if let Err(e) = cors::check_actual(cors_cfg, &dummy) {
                return Err(with_cors_context(e, context.clone()));
            }
        }

        let ctx = ReqCtx {
            security,
            tenant_id,
            context: &context,
            upstream_id: selected.entity.id.as_deref(),
            rate_headers,
            cors_cfg: effective_cors.as_ref(),
            inbound_origin,
            response_rules: selected
                .entity
                .headers
                .as_ref()
                .and_then(|h| h.response.as_ref()),
            plugins: &plugins,
        };

        // 7. Header pipeline.
        let websocket = is_websocket_upgrade(&req);
        let request_rules = selected
            .entity
            .headers
            .as_ref()
            .and_then(|h| h.request.as_ref());
        let (scheme, effective_port) =
            effective_scheme_and_port(&target, self.config.allow_http_upstream)?;
        let mut out_headers = build_outbound_headers(
            req.headers(),
            &target.host,
            effective_port,
            scheme,
            websocket,
            self.config.trust_x_forwarded_for,
            request_rules,
            ctx.tenant_id,
        )?;

        self.run_request_plugins(&ctx, &mut out_headers, effective_auth.as_ref())
            .await
            .map_err(|e| map_plugin_error(e, context.clone()))?;

        // 8. WebSocket: tunnel before any body buffering.
        if websocket {
            let server_on_upgrade = req
                .extensions_mut()
                .remove::<hyper::upgrade::OnUpgrade>()
                .ok_or_else(|| ProxyError::ProtocolError {
                    context: context.clone(),
                    detail: "websocket upgrade handle missing from the incoming request".to_owned(),
                })?;
            if !outbound_is_websocket(&out_headers) {
                return Err(ProxyError::ProtocolError {
                    context: context.clone(),
                    detail: "websocket handshake headers were not preserved".to_owned(),
                });
            }
            let out_path = outbound_path(&raw_path);
            let out_query = filter_query(request_uri.query(), &route_entity.match_rule);
            let url = self.build_url(&target, &out_path, out_query.as_deref())?;
            return self
                .proxy_websocket(&ctx, request_id, out_headers, url, server_on_upgrade)
                .await;
        }

        // 9. Body validation + bounded buffering.
        validate_body_headers(req.headers()).map_err(|detail| ProxyError::Validation {
            context: context.clone(),
            detail,
        })?;
        let inbound_len = req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let body_bytes = read_body(req.into_body(), &self.config, &context).await?;
        if let Some(expected) = inbound_len
            && expected != body_bytes.len() as u64
        {
            return Err(ProxyError::Validation {
                context: context.clone(),
                detail: format!(
                    "Content-Length {expected} does not match the received body ({} bytes)",
                    body_bytes.len()
                ),
            });
        }
        if !body_bytes.is_empty()
            && let Ok(v) = HeaderValue::from_str(&body_bytes.len().to_string())
        {
            out_headers.insert(CONTENT_LENGTH, v);
        }

        // 10. Forward.
        let out_path = outbound_path(&raw_path);
        let out_query = filter_query(request_uri.query(), &route_entity.match_rule);
        let url = self.build_url(&target, &out_path, out_query.as_deref())?;
        let timeout = Duration::from_secs(self.config.proxy_timeout_secs);
        let builder = self
            .request_for(method.as_str(), &url)
            .map_err(|detail| ProxyError::internal(context.clone(), detail))?;
        let upstream = tokio::time::timeout(
            timeout,
            builder
                .body_bytes(body_bytes)
                .headers(header_vec(&out_headers))
                .send(),
        )
        .await
        .map_err(|_| ProxyError::RequestTimeout {
            context: context.clone(),
            timeout_secs: self.config.proxy_timeout_secs,
        })?
        .map_err(|e| map_http_error(e, context.clone(), self.config.proxy_timeout_secs))?;

        // 11. Response pipeline (mutations, CORS, plugins, idle-bounded body).
        let (parts, body) = upstream.into_inner().into_parts();
        self.decorate_response(&ctx, request_id, parts, body).await
    }

    /// Choose the target endpoint for a proxied request (ADR-0001 matrix).
    ///
    /// # Errors
    ///
    /// Returns one of the routing errors from the ADR-0001 matrix.
    fn select_endpoint(
        &self,
        upstream: &UpstreamRecord,
        inbound: &HeaderMap,
    ) -> Result<Endpoint, ProxyError> {
        let counter = self.rr.fetch_add(1, Ordering::Relaxed);
        pick_endpoint(
            &upstream.entity.server.endpoints,
            inbound.get(TARGET_HOST_HEADER),
            counter,
        )
    }

    /// Build the upstream URL.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError::ProtocolError`] for non-HTTP-capable schemes and
    /// [`ProxyError::Internal`] when the URL cannot be assembled.
    fn build_url(
        &self,
        endpoint: &Endpoint,
        path: &str,
        query: Option<&str>,
    ) -> Result<String, ProxyError> {
        let (scheme, port) = effective_scheme_and_port(endpoint, self.config.allow_http_upstream)?;
        let authority = format_authority(&endpoint.host, port, scheme);
        let mut base = format!("{scheme}://{authority}{path}");
        if let Some(q) = query
            && !q.is_empty()
        {
            base.push('?');
            base.push_str(q);
        }
        Url::parse(&base)
            .map(|u| u.to_string())
            .map_err(|e| ProxyError::internal(ProxyContext::default(), format!("url parse: {e}")))
    }

    /// Pick the verb-specific request builder factory.
    ///
    /// # Errors
    ///
    /// Returns an internal error message for methods outside the protocol.
    fn request_for(&self, method: &str, url: &str) -> Result<RequestBuilder, String> {
        match method {
            "GET" => Ok(self.http.get(url)),
            "POST" => Ok(self.http.post(url)),
            "PUT" => Ok(self.http.put(url)),
            "PATCH" => Ok(self.http.patch(url)),
            "DELETE" => Ok(self.http.delete(url)),
            _ => Err(format!("unsupported proxy method {method:?}")),
        }
    }

    /// Run the auth -> guards -> transforms chain for the outbound request.
    ///
    /// # Errors
    ///
    /// Returns the [`PluginError`] from the first failing plugin.
    async fn run_request_plugins(
        &self,
        ctx: &ReqCtx<'_>,
        headers: &mut HeaderMap,
        auth_binding: Option<&AuthConfig>,
    ) -> Result<(), PluginError> {
        // Auth binding first (injects credentials).
        if let Some(binding) = auth_binding {
            let Some(plugin) = self.auth.resolve(&binding.plugin_type) else {
                return Err(PluginError::Config {
                    detail: format!(
                        "auth plugin {:?} (upstream {:?}) is not executable",
                        binding.plugin_type, ctx.upstream_id
                    ),
                });
            };
            let mut plugin_ctx = RequestCtx {
                security: ctx.security,
                config: &binding.config,
                headers,
                tenant_id: ctx.tenant_id,
            };
            plugin.authenticate(&mut plugin_ctx).await?;
        }
        // Then guards, then transforms, each with its per-plugin config.
        for item in ctx.plugins {
            let (id, config) = item.as_ref();
            let mut plugin_ctx = RequestCtx {
                security: ctx.security,
                config,
                headers,
                tenant_id: ctx.tenant_id,
            };
            if let Some(plugin) = self.guards.resolve(id) {
                plugin.guard_request(&mut plugin_ctx).await?;
                continue;
            }
            if let Some(plugin) = self.transforms.resolve(id) {
                plugin.on_request(&mut plugin_ctx).await?;
                continue;
            }
            return Err(PluginError::NotFound {
                plugin_ref: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Run response-phase plugins (transforms then guards), each with its
    /// per-plugin config.
    ///
    /// # Errors
    ///
    /// Returns the [`PluginError`] from the first failing plugin.
    async fn run_response_plugins(
        &self,
        ctx: &ReqCtx<'_>,
        headers: &mut HeaderMap,
        status: StatusCode,
    ) -> Result<(), PluginError> {
        for item in ctx.plugins {
            let (id, config) = item.as_ref();
            let mut plugin_ctx = ResponseCtx {
                security: ctx.security,
                config,
                headers,
                status,
            };
            if let Some(plugin) = self.transforms.resolve(id) {
                plugin.on_response(&mut plugin_ctx).await?;
                continue;
            }
            if let Some(plugin) = self.guards.resolve(id) {
                plugin.guard_response(&mut plugin_ctx).await?;
                continue;
            }
            return Err(PluginError::NotFound {
                plugin_ref: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Decorate a relayed upstream response: header mutations, hop-by-hop
    /// stripping, request-id propagation, CORS enrichment, ADR-0007 source
    /// annotation, rate-limit headers, response plugins and the
    /// idle-bounded body wrapper.
    ///
    /// # Errors
    ///
    /// Returns a data-plane error when response-phase plugins reject it.
    async fn decorate_response(
        &self,
        ctx: &ReqCtx<'_>,
        request_id: &http::HeaderValue,
        mut parts: http::response::Parts,
        body: toolkit_http::ResponseBody,
    ) -> Result<Response<axum::body::Body>, ProxyError> {
        apply_mutations(
            &mut parts.headers,
            &header_mutations(None, ctx.response_rules),
            MutationWarnCtx {
                tenant_id: ctx.tenant_id,
                phase: "response",
            },
        );
        strip_hop_by_hop(&mut parts.headers);
        if !parts.headers.contains_key("x-request-id") {
            parts.headers.insert("x-request-id", request_id.clone());
        }
        if let Some(cfg) = ctx.cors_cfg
            && let Some(origin) = &ctx.inbound_origin
        {
            let dummy = Request::builder()
                .header(ORIGIN, origin.clone())
                .body(())
                .unwrap_or_else(|_| Request::new(()));
            cors::enrich_response(cfg, &dummy, &mut parts.headers);
        }
        // A relayed response originates upstream, whether success or error
        // (ADR-0007: `upstream` for passthrough, `gateway` for OAGW-generated).
        parts.headers.insert(
            "x-oagw-error-source",
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        if let Some((limit, remaining)) = ctx.rate_headers {
            if let Ok(v) = HeaderValue::from_str(&limit.to_string()) {
                parts.headers.insert("x-ratelimit-limit", v);
            }
            if let Ok(v) = HeaderValue::from_str(&remaining.to_string()) {
                parts.headers.insert("x-ratelimit-remaining", v);
            }
        }
        self.run_response_plugins(ctx, &mut parts.headers, parts.status)
            .await
            .map_err(|e| map_plugin_error(e, ctx.context.clone()))?;
        let wrapped = axum::body::Body::new(IdleTimeoutBody::new(
            body,
            Duration::from_secs(self.config.proxy_timeout_secs),
            request_id.to_str().unwrap_or("unknown").to_owned(),
            ctx.tenant_id,
        ));
        Ok(Response::from_parts(parts, wrapped))
    }

    /// Tunnel a WebSocket upgrade through the raw upgraded connections:
    /// hyper's client-side `Upgraded` (from the 101 response) and hyper's
    /// server-side `Upgraded` (from the `OnUpgrade` request extension) are
    /// relayed with `copy_bidirectional` until either side closes.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyError::RequestTimeout`] when the upstream takes too long
    /// to answer, [`ProxyError::ConnectionTimeout`] for transport failures and
    /// [`ProxyError::ProtocolError`] when either side refuses the upgrade.
    async fn proxy_websocket(
        &self,
        ctx: &ReqCtx<'_>,
        request_id: &http::HeaderValue,
        out_headers: HeaderMap,
        url: String,
        server_on_upgrade: hyper::upgrade::OnUpgrade,
    ) -> Result<Response<axum::body::Body>, ProxyError> {
        let timeout = Duration::from_secs(self.config.proxy_timeout_secs);
        let builder = self
            .http
            .get(&url)
            .body_bytes(Bytes::new())
            .headers(header_vec(&out_headers));
        let upstream = tokio::time::timeout(timeout, builder.send())
            .await
            .map_err(|_| ProxyError::RequestTimeout {
                context: ctx.context.clone(),
                timeout_secs: self.config.proxy_timeout_secs,
            })?
            .map_err(|e| map_http_error(e, ctx.context.clone(), self.config.proxy_timeout_secs))?;

        let mut resp = upstream.into_inner();
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            // Upstream declined the upgrade; relay the response through the
            // normal response pipeline.
            let (parts, body) = resp.into_parts();
            return self.decorate_response(ctx, request_id, parts, body).await;
        }

        // Upstream accepted the upgrade. The relay needs the raw upgraded
        // connection, so extract hyper's OnUpgrade **before** the response is
        // torn into parts (the handle lives in the response extensions).
        let client_side =
            hyper::upgrade::on(&mut resp)
                .await
                .map_err(|_| ProxyError::ProtocolError {
                    context: ctx.context.clone(),
                    detail: "websocket upgrade from upstream failed".to_owned(),
                })?;
        let (parts, _body) = resp.into_parts();

        // Server side: hyper completes the upgrade once we return the 101
        // response; await the OnUpgrade in the relay task.
        tokio::spawn(async move {
            if let Ok(server_side) = server_on_upgrade.await {
                let mut client_stream = TokioIo::new(client_side);
                let mut server_stream = TokioIo::new(server_side);
                if let Err(e) =
                    tokio::io::copy_bidirectional(&mut client_stream, &mut server_stream).await
                {
                    tracing::debug!(error = %e, "websocket relay closed with an error");
                }
            }
        });

        // A relayed 101 keeps the upstream handshake headers verbatim (no
        // hop-by-hop stripping — the upgrade headers are load-bearing) and
        // stays flagged `upstream` (ADR-0007).
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *response.headers_mut() = parts.headers;
        if !response.headers().contains_key("x-request-id") {
            response
                .headers_mut()
                .insert("x-request-id", request_id.clone());
        }
        response.headers_mut().insert(
            "x-oagw-error-source",
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        if let Some((limit, remaining)) = ctx.rate_headers {
            if let Ok(v) = HeaderValue::from_str(&limit.to_string()) {
                response.headers_mut().insert("x-ratelimit-limit", v);
            }
            if let Ok(v) = HeaderValue::from_str(&remaining.to_string()) {
                response.headers_mut().insert("x-ratelimit-remaining", v);
            }
        }
        self.run_response_plugins(ctx, response.headers_mut(), StatusCode::SWITCHING_PROTOCOLS)
            .await
            .map_err(|e| map_plugin_error(e, ctx.context.clone()))?;
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Whether an inbound request is a WebSocket upgrade request.
#[must_use]
fn is_websocket_upgrade(req: &Request<axum::body::Body>) -> bool {
    connection_tokens(req.headers()).any(|t| t.eq_ignore_ascii_case("upgrade"))
        && header_equals(req.headers(), "upgrade", "websocket")
}

/// Tokens listed in the `Connection` header.
fn connection_tokens(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get(CONNECTION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn header_equals(headers: &HeaderMap, name: &str, want: &str) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case(want))
}

/// Whether an outbound header map requests a WebSocket upgrade.
#[must_use]
fn outbound_is_websocket(headers: &HeaderMap) -> bool {
    connection_tokens(headers).any(|t| t.eq_ignore_ascii_case("upgrade"))
        && header_equals(headers, "upgrade", "websocket")
}

/// `host[:port]`, bracketing IPv6 literals and omitting the port when it
/// equals the scheme default.
#[must_use]
fn format_authority(host: &str, port: u16, scheme: &str) -> String {
    let default = match scheme {
        "http" | "ws" => 80,
        _ => 443,
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if port == default {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// Inbound headers as `(name, value)` pairs (for the toolkit-http builder).
///
/// The toolkit client's public builder API carries header values as `String`,
/// so a value that is not valid UTF-8 (obs-text, e.g. `0x80`) is not
/// representable on the wire: such a header is SKIPPED with a warning rather
/// than silently forwarded as an empty string (which would corrupt the
/// upstream's view of the value). Byte-exact relaying of valid values is
/// guaranteed by this pass (values are copied as bytes, untouched).
#[must_use]
fn header_vec(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(headers.len());
    for (n, v) in headers {
        let name = n.as_str().to_owned();
        match v.to_str() {
            // Byte-exact copy: valid values are forwarded untouched (including
            // intentionally empty ones, which are legal HTTP).
            Ok(value) => out.push((name, value.to_owned())),
            Err(_) => {
                // Not valid UTF-8 (e.g. obs-text) and therefore not
                // representable through the client's String-typed API: skipping
                // with a warning beats silently forwarding an empty string that
                // would corrupt the upstream's view of the header.
                tracing::warn!(
                    event = "oagw.proxy.header_unrepresentable",
                    name = %name,
                    "skipping a header whose value is not valid UTF-8 (cannot be carried by the HTTP client)"
                );
            }
        }
    }
    out
}

/// Strip hop-by-hop headers (and `Connection`-named extra tokens), optionally
/// preserving the WebSocket handshake set on upgrade requests.
fn strip_hop_by_hop_except(headers: &mut HeaderMap, preserve_websocket: bool) {
    let mut also: Vec<String> = Vec::new();
    for token in connection_tokens(headers) {
        also.push(token.to_ascii_lowercase());
    }
    for name in HOP_BY_HOP {
        if !(preserve_websocket && WEBSOCKET_HANDSHAKE.contains(name)) {
            headers.remove(*name);
        }
    }
    for extra in also {
        if !(preserve_websocket && WEBSOCKET_HANDSHAKE.contains(&extra.as_str()))
            && let Ok(name) = HeaderName::from_bytes(extra.as_bytes())
        {
            headers.remove(&name);
        }
    }
}

/// Strip hop-by-hop headers (and `Connection`-named extra tokens).
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    strip_hop_by_hop_except(headers, false);
}

/// Apply the passthrough policy: hop-by-hop + routing headers never forward;
/// the always-forward well-known set survives every mode; WebSocket
/// handshake headers are preserved verbatim on upgrade requests.
///
/// The gateway's `x-request-id` and (when proxies are untrusted) inbound
/// `x-forwarded-for` are never forwarded by this pass — the former is re-added
/// by the caller from the seeded value, the latter is rewritten from the
/// peer address.
fn forward_headers(
    inbound: &HeaderMap,
    out: &mut HeaderMap,
    mode: Option<PassthroughMode>,
    allowlist: &[String],
    websocket: bool,
    trust_x_forwarded_for: bool,
) {
    let mode = mode.unwrap_or(PassthroughMode::None);
    let excluded: Vec<String> = connection_tokens(inbound)
        .map(str::to_ascii_lowercase)
        .collect();
    for (name, value) in inbound {
        let name_str = name.as_str();
        if name_str == HOST.as_str() || name_str == TARGET_HOST_HEADER || name_str == "x-request-id"
        {
            continue;
        }
        if !trust_x_forwarded_for && name_str == "x-forwarded-for" {
            continue;
        }
        let is_hop = HOP_BY_HOP.contains(&name_str) || excluded.iter().any(|t| t == name_str);
        if is_hop && !(websocket && WEBSOCKET_HANDSHAKE.contains(&name_str)) {
            continue;
        }
        if name_str == CONTENT_LENGTH.as_str() {
            // Recomputed after body buffering.
            continue;
        }
        let allowed = (websocket && WEBSOCKET_HANDSHAKE.contains(&name_str))
            || mode == PassthroughMode::All
            || (mode == PassthroughMode::Allowlist
                && allowlist.iter().any(|a| a.eq_ignore_ascii_case(name_str)))
            || (mode == PassthroughMode::None && ALWAYS_FORWARD.contains(&name_str));
        if allowed
            && let (Ok(header_name), Ok(header_value)) = (
                HeaderName::from_bytes(name.as_str().as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            )
        {
            out.append(header_name, header_value);
        }
    }
}

/// Build the outbound request headers from the inbound request.
///
/// # Errors
///
/// Returns [`ProxyError::Internal`] when the Host header cannot be built.
#[allow(clippy::too_many_arguments)]
fn build_outbound_headers(
    inbound: &HeaderMap,
    host: &str,
    port: u16,
    scheme: &str,
    websocket: bool,
    trust_x_forwarded_for: bool,
    rules: Option<&RequestHeaders>,
    tenant_id: Uuid,
) -> Result<HeaderMap, ProxyError> {
    let mut out = HeaderMap::new();
    forward_headers(
        inbound,
        &mut out,
        rules.and_then(|r| r.passthrough),
        &rules
            .map(|r| r.passthrough_allowlist.clone())
            .unwrap_or_default(),
        websocket,
        trust_x_forwarded_for,
    );
    if let Some(rules) = rules {
        apply_mutations(
            &mut out,
            &header_mutations(Some(rules), None),
            MutationWarnCtx {
                tenant_id,
                phase: "request",
            },
        );
        // Mutations can reintroduce hop-by-hop headers (e.g. `set:
        // {"connection": "keep-alive"}`): re-run the exclusion so the outbound
        // request never carries a header the receiving intermediary would
        // consume instead of the intended endpoint (F-013b).
        strip_hop_by_hop_except(&mut out, websocket);
    }
    // The gateway's request id is always propagated upstream so the whole
    // chain shares one stable value (DESIGN observability).
    if let Some(value) = inbound.get("x-request-id") {
        out.insert("x-request-id", value.clone());
    }
    let authority = format_authority(host, port, scheme);
    out.insert(
        HOST,
        HeaderValue::from_str(&authority).map_err(|e| {
            ProxyError::internal(ProxyContext::default(), format!("host header: {e}"))
        })?,
    );
    Ok(out)
}

/// Individual mutation sets (request- or response-side).
#[derive(Default)]
struct HeaderMutations {
    remove: Vec<String>,
    set: BTreeMap<String, String>,
    add: BTreeMap<String, String>,
}

/// Collect the request-side or response-side mutation sets.
fn header_mutations(
    request: Option<&RequestHeaders>,
    response: Option<&ResponseHeaders>,
) -> HeaderMutations {
    match (request, response) {
        (Some(r), _) => HeaderMutations {
            remove: r.remove.clone(),
            set: r.set.clone(),
            add: r.add.clone(),
        },
        (None, Some(r)) => HeaderMutations {
            remove: r.remove.clone(),
            set: r.set.clone(),
            add: r.add.clone(),
        },
        (None, None) => HeaderMutations::default(),
    }
}

/// Tenant + phase context for mutation-skip warnings (A5).
#[derive(Clone, Copy)]
struct MutationWarnCtx {
    tenant_id: Uuid,
    phase: &'static str,
}

/// Apply remove-then-set-then-add mutations to a header map.
///
/// Mutations whose name or value is not representable as a real HTTP header
/// are skipped with a `tracing::warn!` carrying the tenant context. Store-time
/// validation (validate_headers_shape) is the primary guard; this is the
/// last-line defense for configs that slipped through or were written by
/// older control planes.
fn apply_mutations(headers: &mut HeaderMap, mutations: &HeaderMutations, warn: MutationWarnCtx) {
    for name in &mutations.remove {
        if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&header_name);
        } else {
            tracing::warn!(
                event = "oagw.proxy.header_mutation_skipped",
                tenant_id = %warn.tenant_id,
                phase = warn.phase,
                name = %name,
                "skipping a header removal with an invalid HTTP header name"
            );
        }
    }
    for (name, value) in &mutations.set {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(header_name), Ok(header_value)) => {
                headers.insert(header_name, header_value);
            }
            _ => {
                tracing::warn!(
                    event = "oagw.proxy.header_mutation_skipped",
                    tenant_id = %warn.tenant_id,
                    phase = warn.phase,
                    name = %name,
                    "skipping a header mutation whose name or value is not a valid HTTP header"
                );
            }
        }
    }
    for (name, value) in &mutations.add {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(header_name), Ok(header_value)) => {
                headers.append(header_name, header_value);
            }
            _ => {
                tracing::warn!(
                    event = "oagw.proxy.header_mutation_skipped",
                    tenant_id = %warn.tenant_id,
                    phase = warn.phase,
                    name = %name,
                    "skipping a header mutation whose name or value is not a valid HTTP header"
                );
            }
        }
    }
}

/// Validate the inbound transfer-encoding / body framing headers.
fn validate_body_headers(headers: &HeaderMap) -> Result<(), String> {
    if let Some(te) = headers.get(TRANSFER_ENCODING) {
        let te_str = te.to_str().unwrap_or_default();
        let ok = te_str
            .split(',')
            .map(str::trim)
            .all(|t| t.eq_ignore_ascii_case("chunked"));
        if !ok {
            return Err(format!(
                "unsupported Transfer-Encoding {te_str:?} (only chunked or absent)"
            ));
        }
    }
    Ok(())
}

/// Read and validate the request body into bytes.
///
/// The size budget is enforced incrementally as frames arrive, so an
/// oversized body is detected exactly at the boundary without relying on a
/// downstream error type for classification.
///
/// # Errors
///
/// Returns [`ProxyError::PayloadTooLarge`] when the body exceeds the hard
/// limit, [`ProxyError::Validation`] when the body cannot be read.
async fn read_body(
    body: axum::body::Body,
    config: &OagwConfig,
    context: &ProxyContext,
) -> Result<Bytes, ProxyError> {
    let mut stream = body.into_data_stream();
    let mut remaining = config.max_body_size_bytes;
    let mut out = Vec::with_capacity(remaining.clamp(1, 64 * 1024));
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ProxyError::Validation {
            context: context.clone(),
            detail: format!("failed to read request body: {e}"),
        })?;
        if chunk.len() > remaining {
            return Err(ProxyError::PayloadTooLarge {
                context: context.clone(),
            });
        }
        remaining -= chunk.len();
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

/// Rebuild the `/`-leading path used for route matching.
///
/// The path suffix captured by the handler's wildcard already carries a
/// leading `/`, so stray leading slashes are collapsed before a single `/`
/// is prepended. The empty suffix means "the route path itself".
#[must_use]
fn normalize_request_path(raw: &str) -> String {
    match raw.trim_start_matches('/') {
        "" => "/".to_owned(),
        trimmed => format!("/{trimmed}"),
    }
}

/// Rebuild the `/`-leading outbound path for the wire.
#[must_use]
fn outbound_path(raw: &str) -> String {
    match raw.trim_start_matches('/') {
        "" => "/".to_owned(),
        trimmed => format!("/{trimmed}"),
    }
}

/// Route match: owning upstream + method allowlist + longest path prefix +
/// suffix mode + explicit priority (enabled routes only).
#[must_use]
fn select_route<'a>(
    routes: &'a [RouteRecord],
    chain: &[Uuid],
    upstream_id: Option<Uuid>,
    method: &str,
    request_path: &str,
) -> Option<&'a RouteRecord> {
    let mut best: Option<(&RouteRecord, usize, usize, u64)> = None;
    for record in routes {
        if !record.entity.enabled {
            continue;
        }
        let Some(http) = record.entity.match_rule.http.as_ref() else {
            continue;
        };
        let route_upstream = gts::parse_resource_id(&record.entity.upstream_id);
        if upstream_id.is_some() && route_upstream != upstream_id {
            continue;
        }
        if !http.methods.iter().any(|m| m.matches(method)) {
            continue;
        }
        if !path_prefix_matches(&http.path, request_path) {
            continue;
        }
        let suffix = suffix_beyond_prefix(&http.path, request_path);
        if !suffix.is_empty() && http.path_suffix_mode == PathSuffixMode::Disabled {
            continue;
        }
        let idx = chain
            .iter()
            .position(|t| *t == record.tenant_id)
            .unwrap_or(usize::MAX);
        let len = http.path.len();
        let priority = record.entity.priority;
        let is_better = match best {
            None => true,
            Some((_, best_idx, best_len, best_priority)) => {
                idx < best_idx
                    || (idx == best_idx
                        && (len > best_len || (len == best_len && priority > best_priority)))
            }
        };
        if is_better {
            best = Some((record, idx, len, priority));
        }
    }
    best.map(|(record, _, _, _)| record)
}

/// Segment-aligned path prefix match (`/v1` matches `/v1` and `/v1/x`, not
/// `/v10`).
#[must_use]
fn path_prefix_matches(prefix: &str, request_path: &str) -> bool {
    request_path == prefix
        || (request_path.starts_with(prefix)
            && (prefix.ends_with('/') || request_path.as_bytes().get(prefix.len()) == Some(&b'/')))
}

/// The request path remainder after `prefix`, or `""` when equal or shorter.
#[must_use]
fn suffix_beyond_prefix<'a>(prefix: &str, request_path: &'a str) -> &'a str {
    match request_path.len().checked_sub(prefix.len()) {
        Some(0) | None => "",
        Some(_) => &request_path[prefix.len()..],
    }
}

/// Filter the incoming raw query string against the route's allowlist.
///
/// Only allowlisted parameter names survive; an empty allowlist drops every
/// parameter (nothing is explicitly allowed — DESIGN "reject if unknown").
#[must_use]
fn filter_query(raw: Option<&str>, match_rule: &MatchRule) -> Option<String> {
    let http = match_rule.http.as_ref()?;
    let allowlist = &http.query_allowlist;
    let raw = raw?;
    if allowlist.is_empty() {
        return None;
    }
    let kept: Vec<&str> = raw
        .split('&')
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("");
            !name.is_empty() && allowlist.iter().any(|a| a == name)
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("&"))
    }
}

/// Merge multiple rate-limit policies into one effective policy: minimum
/// tokens-per-second and minimum capacity win; the merged policy is expressed
/// on a per-second window (DESIGN "Effective Rate Limit").
///
/// The tokens-per-second merge is inherently floating point; the f64/u64
/// round-trip is intentional and the result is clamped to at least 1.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn merge_rate_limits(policies: &[RateLimit]) -> Option<RateLimit> {
    if policies.is_empty() {
        return None;
    }
    let tokens_per_sec = policies
        .iter()
        .map(|p| p.sustained.rate as f64 / p.sustained.window.as_secs() as f64)
        .fold(f64::INFINITY, f64::min);
    let capacity = policies
        .iter()
        .map(RateLimit::capacity)
        .min()
        .unwrap_or(1)
        .max(1);
    let cost = policies.iter().map(|p| p.cost).max().unwrap_or(1).max(1);
    let rank = policies
        .iter()
        .map(|p| strategy_rank(p.strategy))
        .min()
        .unwrap_or(0);
    let rate = (tokens_per_sec.ceil() as u64).max(1);
    Some(RateLimit {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: RateWindow::Second,
        },
        burst: Some(BurstCapacity { capacity }),
        scope: RateScope::Tenant,
        strategy: rank_to_strategy(rank),
        cost,
    })
}

fn strategy_rank(s: RateStrategy) -> u8 {
    match s {
        RateStrategy::Reject => 0,
        RateStrategy::Queue => 1,
        RateStrategy::Degrade => 2,
    }
}

fn rank_to_strategy(rank: u8) -> RateStrategy {
    match rank {
        0 => RateStrategy::Reject,
        1 => RateStrategy::Queue,
        _ => RateStrategy::Degrade,
    }
}

/// Rate-limiter bucket key for a scope.
///
/// `Global` / `Tenant` / `Route` keys reuse [`rate_scope_key`] so management
/// deletions evict exactly the buckets the data plane touches; `User`/`Ip`
/// keys are tenant-namespaced (they cannot be precomputed at deletion time).
#[must_use]
fn rate_key(
    scope: RateScope,
    security: &SecurityContext,
    tenant_id: Uuid,
    ip: &str,
    route_id: Option<&str>,
) -> String {
    let fallback = route_id.unwrap_or("unknown");
    if let Some(key) = rate_scope_key(scope, tenant_id, fallback) {
        return key;
    }
    match scope {
        RateScope::User => format!("u:{tenant_id}:{}", security.subject_id()),
        RateScope::Ip => format!("i:{tenant_id}:{ip}"),
        RateScope::Global | RateScope::Tenant | RateScope::Route => {
            rate_scope_key(scope, tenant_id, fallback).unwrap_or_default()
        }
    }
}

/// Best available client address for IP-scoped rate limiting and
/// diagnostics: the first `X-Forwarded-For` hop only when the proxy chain is
/// trusted, otherwise the socket peer address (from `ConnectInfo`) or
/// `unknown`.
#[must_use]
fn client_ip(req: &Request<axum::body::Body>, trust_x_forwarded_for: bool) -> String {
    if trust_x_forwarded_for {
        return req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| "unknown".to_owned(), str::to_owned);
    }
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map_or_else(|| "unknown".to_owned(), |c| c.0.ip().to_string())
}

/// The effective auth binding: the selected upstream's, unless an ancestor
/// `enforce` overrides it (the root-most enforced ancestor wins).
#[must_use]
fn effective_auth_binding(
    chain: &[UpstreamRecord],
    selected: &UpstreamRecord,
) -> Option<AuthConfig> {
    let mut binding = selected.entity.auth.clone();
    for ancestor in chain.iter().filter(|r| r.tenant_id != selected.tenant_id) {
        if let Some(a) = &ancestor.entity.auth
            && a.sharing == SharingMode::Enforce
        {
            binding = Some(a.clone());
        }
    }
    binding
}

/// The effective plugin chain: enforced-ancestor items, then the selected
/// upstream's items, then the route's items — each with its per-plugin config.
#[must_use]
fn effective_plugins(
    chain: &[UpstreamRecord],
    selected: &UpstreamRecord,
    route: &Route,
) -> Vec<PluginBindingItem> {
    let mut items = Vec::new();
    for ancestor in chain.iter().filter(|r| r.tenant_id != selected.tenant_id) {
        if let Some(binding) = &ancestor.entity.plugins
            && binding.sharing == SharingMode::Enforce
        {
            items.extend(binding.items.iter().cloned());
        }
    }
    if let Some(binding) = &selected.entity.plugins {
        items.extend(binding.items.iter().cloned());
    }
    if let Some(binding) = &route.plugins {
        items.extend(binding.items.iter().cloned());
    }
    items
}

/// The effective CORS configuration assembled along the tenant chain.
///
/// Ancestor `enforce` policies replace the accumulated set wholesale;
/// ancestor `inherit` policies union in; the selected upstream's own config
/// always participates (its `sharing` governs its descendants, not itself);
/// a route-level config is the final override point — `inherit` unions, any
/// other sharing replaces wholesale.
#[must_use]
fn effective_cors_config(
    chain: &[UpstreamRecord],
    selected: &UpstreamRecord,
    route_cors: Option<&CorsConfig>,
) -> Option<CorsConfig> {
    let mut acc: Option<CorsConfig> = None;
    for record in chain.iter().filter(|r| r.tenant_id != selected.tenant_id) {
        if let Some(c) = &record.entity.cors {
            acc = match c.sharing {
                SharingMode::Enforce => Some(c.clone()),
                SharingMode::Inherit => Some(merge_cors(acc.as_ref(), c)),
                SharingMode::Private => acc,
            };
        }
    }
    if let Some(c) = &selected.entity.cors {
        acc = Some(merge_cors(acc.as_ref(), c));
    }
    if let Some(c) = route_cors {
        acc = Some(if c.sharing == SharingMode::Inherit {
            merge_cors(acc.as_ref(), c)
        } else {
            c.clone()
        });
    }
    acc
}

/// Union two CORS configs (origins, methods and expose-headers combine;
/// enabled and allow-credentials are combined with a boolean OR).
#[must_use]
fn merge_cors(base: Option<&CorsConfig>, add: &CorsConfig) -> CorsConfig {
    let mut out = base.cloned().unwrap_or_default();
    out.enabled = out.enabled || add.enabled;
    push_unique(&mut out.allowed_origins, &add.allowed_origins);
    push_unique(&mut out.allowed_methods, &add.allowed_methods);
    push_unique(&mut out.expose_headers, &add.expose_headers);
    out.allow_credentials = out.allow_credentials || add.allow_credentials;
    // The merged config is synthetic; sharing is only meaningful on inputs.
    out.sharing = SharingMode::Private;
    out
}

/// Append `source` items to `target` when not already present (case-folded).
fn push_unique(target: &mut Vec<String>, source: &[String]) {
    for item in source {
        if !target.iter().any(|t| t.eq_ignore_ascii_case(item)) {
            target.push(item.clone());
        }
    }
}

/// Map a control-plane error onto a proxy error.
#[must_use]
fn map_domain_to_proxy(e: &DomainError, context: ProxyContext) -> ProxyError {
    match e {
        DomainError::NotFound => ProxyError::RouteNotFound { context },
        DomainError::Validation { .. } | DomainError::Conflict { .. } => {
            ProxyError::validation("upstream resolution failed for the requested alias")
                .with_context(context)
        }
        _ => ProxyError::internal(context, "control plane failure during routing"),
    }
}

/// Map an HTTP send failure onto a proxy error.
#[must_use]
fn map_http_error(e: HttpError, context: ProxyContext, timeout_secs: u64) -> ProxyError {
    match e {
        HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => ProxyError::RequestTimeout {
            context,
            timeout_secs,
        },
        HttpError::Transport(_)
        | HttpError::Tls(_)
        | HttpError::Overloaded
        | HttpError::ServiceClosed
        | HttpError::InvalidUri { .. }
        | HttpError::InvalidScheme { .. }
        | HttpError::InsecureTransport => ProxyError::ConnectionTimeout { context },
        HttpError::BodyTooLarge { .. } => ProxyError::PayloadTooLarge { context },
        e @ (HttpError::RequestBuild(_)
        | HttpError::InvalidHeaderName(_)
        | HttpError::InvalidHeaderValue(_)) => {
            ProxyError::internal(context, format!("outbound request build failed: {e}"))
        }
        e @ (HttpError::HttpStatus { .. } | HttpError::Json(_) | HttpError::FormEncode(_)) => {
            ProxyError::DownstreamError {
                context,
                detail: format!("upstream request failed: {e}"),
            }
        }
        other => ProxyError::internal(context, format!("upstream request failed: {other}")),
    }
}

/// Map a plugin failure onto a proxy error (fail closed).
///
/// Config errors are logged on the gateway (with the request context) but
/// NEVER echoed to the caller: a `PluginError::Config` detail string is
/// control-plane internals (e.g. an unresolved `cred://` reference or a
/// plugin registry gap) and would leak configuration surface to the client,
/// so it surfaces as a sanitized `500 Internal` (F-008 gateway error
/// boundary). Unresolvable plugin references are `503 PluginNotFound`;
/// internal failures are `500` and never leak their diagnostic.
#[must_use]
fn map_plugin_error(e: PluginError, context: ProxyContext) -> ProxyError {
    match e {
        PluginError::Reject { status, detail, .. } => {
            if status == 400 {
                ProxyError::Validation { context, detail }
            } else {
                ProxyError::DownstreamError { context, detail }
            }
        }
        PluginError::AuthFailed { detail } => ProxyError::AuthenticationFailed { context, detail },
        PluginError::Secret { reference } => ProxyError::SecretNotFound { context, reference },
        PluginError::Config { detail } => {
            tracing::error!(
                event = "oagw.proxy.plugin_config_error",
                upstream_id = %context.upstream_id.as_ref().map_or("unknown", String::as_str),
                host = %context.host.as_ref().map_or("unknown", String::as_str),
                path = %context.path.as_ref().map_or("unknown", String::as_str),
                error = %detail,
                "upstream plugin configuration error hidden from the caller"
            );
            ProxyError::internal(context, "upstream plugin configuration error")
        }
        PluginError::NotFound { plugin_ref } => ProxyError::PluginNotFound {
            context,
            plugin_ref,
        },
        PluginError::Internal { .. } => ProxyError::internal(context, "plugin failure"),
    }
}

/// Fold entity context into a proxy context.
#[must_use]
fn with_entity_context(context: &ProxyContext, selected: &UpstreamRecord) -> ProxyContext {
    let mut c = context.clone();
    if c.upstream_id.is_none() {
        c.upstream_id.clone_from(&selected.entity.id);
    }
    if c.host.is_none() {
        c.host = selected
            .entity
            .server
            .endpoints
            .first()
            .map(|e| e.host.clone());
    }
    c
}

/// Re-raise a CORS proxy error with the request context folded in.
#[must_use]
fn with_cors_context(e: ProxyError, context: ProxyContext) -> ProxyError {
    match e {
        ProxyError::CorsOriginNotAllowed { origin, .. } => {
            ProxyError::CorsOriginNotAllowed { context, origin }
        }
        ProxyError::CorsMethodNotAllowed { method, .. } => {
            ProxyError::CorsMethodNotAllowed { context, method }
        }
        other => other.with_context(context),
    }
}

fn bare_request<B>(req: &Request<B>) -> Request<()> {
    let mut dummy = Request::new(());
    *dummy.method_mut() = req.method().clone();
    *dummy.headers_mut() = req.headers().clone();
    dummy
}

/// Resolve the effective outbound wire scheme for an endpoint: `http`/`https`
/// only (never `ws`/`wss` — the toolkit client performs the TLS handshake for
/// secure endpoints and the Upgrade rides on a plain GET).
///
/// # Errors
///
/// Returns [`ProxyError::ProtocolError`] for non-HTTP-capable schemes.
fn effective_scheme(
    endpoint: &Endpoint,
    allow_http_upstream: bool,
) -> Result<&'static str, ProxyError> {
    match &endpoint.scheme {
        EndpointScheme::Https | EndpointScheme::Wss if allow_http_upstream => Ok("http"),
        EndpointScheme::Https | EndpointScheme::Wss => Ok("https"),
        EndpointScheme::Wt | EndpointScheme::Grpc => Err(ProxyError::ProtocolError {
            context: ProxyContext::default(),
            detail: format!(
                "scheme {:?} is not proxiable today",
                endpoint.scheme.as_str()
            ),
        }),
    }
}

/// Resolve the effective outbound wire scheme and port together so a
/// downgraded scheme (F-021: `allow_http_upstream`) carries its plaintext
/// default port when the endpoint declares none — a downgraded endpoint must
/// never be addressed as `http://host:443`.
///
/// # Errors
///
/// Returns [`ProxyError::ProtocolError`] for non-HTTP-capable schemes.
fn effective_scheme_and_port(
    endpoint: &Endpoint,
    allow_http_upstream: bool,
) -> Result<(&'static str, u16), ProxyError> {
    let scheme = effective_scheme(endpoint, allow_http_upstream)?;
    let port = endpoint.effective_port_with_scheme(scheme);
    Ok((scheme, port))
}

/// Time elapsed since the Unix epoch (0 on a bogus clock).
#[must_use]
fn duration_since_epoch() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

/// ADR-0001 endpoint selection: single endpoint, header-required pools and
/// round-robin.
///
/// # Errors
///
/// Returns [`ProxyError::MissingTargetHost`], [`ProxyError::InvalidTargetHost`]
/// or [`ProxyError::UnknownTargetHost`] per the ADR-0001 matrix.
#[allow(clippy::cast_possible_truncation)]
fn pick_endpoint(
    endpoints: &[Endpoint],
    target_host: Option<&http::HeaderValue>,
    counter: u64,
) -> Result<Endpoint, ProxyError> {
    let context = ProxyContext::default();
    let valid_hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();
    if let Some(raw) = target_host.and_then(|v| v.to_str().ok()) {
        let host = raw.trim();
        if host.is_empty() {
            return Err(ProxyError::InvalidTargetHost {
                context,
                detail: "X-OAGW-Target-Host must not be empty".to_owned(),
            });
        }
        if let Err(detail) = validate_host(host) {
            return Err(ProxyError::InvalidTargetHost { context, detail });
        }
        let normalized = normalize_host(host);
        for ep in endpoints {
            if normalize_host(&ep.host) == normalized {
                return Ok(ep.clone());
            }
        }
        Err(ProxyError::UnknownTargetHost {
            context,
            valid_hosts,
        })
    } else if endpoints.len() == 1 {
        Ok(endpoints[0].clone())
    } else if derive_alias(endpoints).is_some() {
        Err(ProxyError::MissingTargetHost {
            context,
            valid_hosts,
        })
    } else {
        let idx = (counter % endpoints.len() as u64) as usize;
        Ok(endpoints[idx].clone())
    }
}

/// Stable machine-readable error kind for the completion log.
#[must_use]
fn proxy_error_kind(e: &ProxyError) -> &'static str {
    match e {
        ProxyError::RouteNotFound { .. } => "route_not_found",
        ProxyError::Validation { .. } => "validation",
        ProxyError::MissingTargetHost { .. } => "missing_target_host",
        ProxyError::InvalidTargetHost { .. } => "invalid_target_host",
        ProxyError::UnknownTargetHost { .. } => "unknown_target_host",
        ProxyError::AuthenticationFailed { .. } => "authentication_failed",
        ProxyError::CorsOriginNotAllowed { .. } => "cors_origin_not_allowed",
        ProxyError::CorsMethodNotAllowed { .. } => "cors_method_not_allowed",
        ProxyError::PayloadTooLarge { .. } => "payload_too_large",
        ProxyError::RateLimitExceeded { .. } => "rate_limit_exceeded",
        ProxyError::SecretNotFound { .. } => "secret_not_found",
        ProxyError::ProtocolError { .. } => "protocol_error",
        ProxyError::DownstreamError { .. } => "downstream_error",
        ProxyError::StreamAborted { .. } => "stream_aborted",
        ProxyError::LinkUnavailable { .. } => "link_unavailable",
        ProxyError::PluginNotFound { .. } => "plugin_not_found",
        ProxyError::ConnectionTimeout { .. } => "connection_timeout",
        ProxyError::RequestTimeout { .. } => "request_timeout",
        ProxyError::IdleTimeout { .. } => "idle_timeout",
        ProxyError::GrpcNotImplemented { .. } => "grpc_not_implemented",
        ProxyError::Internal { .. } => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port,
        }
    }

    fn cors(origins: &[&str], sharing: SharingMode) -> CorsConfig {
        CorsConfig {
            sharing,
            enabled: true,
            allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    use crate::domain::model::{ServerConfig, Upstream};

    fn upstream_with_cors(tenant: Uuid, cors: Option<CorsConfig>) -> UpstreamRecord {
        UpstreamRecord {
            tenant_id: tenant,
            entity: Upstream {
                id: None,
                enabled: true,
                alias: Some("up".to_owned()),
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: vec![ep("up.example", None)],
                },
                protocol: gts::PROTOCOL_HTTP_ID.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors,
            },
        }
    }

    fn chain(candidates: &[UpstreamRecord], selected: &UpstreamRecord) -> Vec<UpstreamRecord> {
        let mut out = vec![selected.clone()];
        for c in candidates {
            if c.tenant_id != selected.tenant_id {
                out.push(c.clone());
            }
        }
        out
    }

    fn security(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .unwrap_or_else(|_| {
                // Unreachable: the builder input above is always valid.
                panic!("valid security context")
            })
    }

    #[test]
    fn empty_query_allowlist_drops_everything() {
        let rule = MatchRule {
            http: Some(crate::domain::model::HttpMatch {
                methods: vec![crate::domain::model::HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert_eq!(
            filter_query(Some("id=5&utm_source=x"), &rule),
            None,
            "no allowances means no query parameter survives"
        );
    }

    #[test]
    fn query_allowlist_keeps_only_known_names() {
        let rule = MatchRule {
            http: Some(crate::domain::model::HttpMatch {
                methods: vec![crate::domain::model::HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: vec!["id".to_owned()],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert_eq!(
            filter_query(Some("id=5&utm_source=x"), &rule).as_deref(),
            Some("id=5")
        );
        assert_eq!(filter_query(None, &rule), None);
    }

    #[test]
    fn authority_brackets_ipv6_and_maps_ws_default_port() {
        assert_eq!(
            format_authority("2001:db8::1", 443, "https"),
            "[2001:db8::1]"
        );
        assert_eq!(
            format_authority("2001:db8::1", 8443, "https"),
            "[2001:db8::1]:8443"
        );
        // ws downgrades to the plain-HTTP default port convention.
        assert_eq!(format_authority("up.example", 80, "http"), "up.example");
        assert_eq!(
            format_authority("up.example", 8080, "http"),
            "up.example:8080"
        );
        assert_eq!(format_authority("up.example", 443, "https"), "up.example");
    }

    #[test]
    fn cors_inherit_unions_enforce_replaces_and_route_overrides() {
        let root = Uuid::from_u128(0xaaaa_0000_0000_0000_0000_0000_0000_0001);
        let child = Uuid::from_u128(0xaaaa_0000_0000_0000_0000_0000_0000_0002);

        let ancestor_inherit =
            upstream_with_cors(root, Some(cors(&["https://a"], SharingMode::Inherit)));
        // Selected child inherits onto the ancestor's set.
        let selected = upstream_with_cors(child, Some(cors(&["https://b"], SharingMode::Inherit)));
        let effective =
            effective_cors_config(&chain(&[ancestor_inherit], &selected), &selected, None);
        let origins = effective.expect("cors").allowed_origins;
        assert!(
            origins.iter().any(|o| o == "https://a") && origins.iter().any(|o| o == "https://b"),
            "inherit unions, got {origins:?}"
        );

        // An enforced ancestor forces its set in even over the child's inherit.
        let ancestor_enforce =
            upstream_with_cors(root, Some(cors(&["https://forced"], SharingMode::Enforce)));
        let effective = effective_cors_config(
            &chain(std::slice::from_ref(&ancestor_enforce), &selected),
            &selected,
            None,
        );
        let origins = effective.expect("cors").allowed_origins;
        assert!(
            origins.iter().any(|o| o == "https://forced")
                && origins.iter().any(|o| o == "https://b"),
            "enforce forces its origins, got {origins:?}"
        );

        // A route-level config is the final override point: enforce replaces
        // the whole chain wholesale.
        let route_cors = Some(cors(&["https://route"], SharingMode::Enforce));
        let effective = effective_cors_config(
            &chain(&[ancestor_enforce], &selected),
            &selected,
            route_cors.as_ref(),
        );
        let origins = effective.expect("cors").allowed_origins;
        assert_eq!(origins, vec!["https://route".to_owned()]);
    }

    #[test]
    fn rate_keys_align_with_control_plane_eviction() {
        let tenant = Uuid::from_u128(0x1234);
        let sec = security(tenant);
        assert_eq!(
            rate_key(RateScope::Global, &sec, tenant, "1.2.3.4", None),
            "g:"
        );
        assert_eq!(
            rate_key(RateScope::Tenant, &sec, tenant, "1.2.3.4", None),
            format!("t:{tenant}")
        );
        assert_eq!(
            rate_key(RateScope::Route, &sec, tenant, "1.2.3.4", Some("r-uuid")),
            "r:r-uuid"
        );
        // User / Ip keys are tenant-namespaced (not precomputable at deletion).
        let user_key = rate_key(RateScope::User, &sec, tenant, "1.2.3.4", None);
        assert!(user_key.starts_with(&format!("u:{tenant}:")));
        let ip_key = rate_key(RateScope::Ip, &sec, tenant, "1.2.3.4", None);
        assert_eq!(ip_key, format!("i:{tenant}:1.2.3.4"));
    }

    #[test]
    fn client_ip_prefers_peer_unless_forwarding_is_trusted() {
        let mut req = Request::builder()
            .uri("/oagw/v1/proxy/echo/x")
            .header("x-forwarded-for", "203.0.113.9, 10.0.0.1")
            .body(axum::body::Body::empty())
            .expect("valid request");
        assert_eq!(
            client_ip(&req, true),
            "203.0.113.9",
            "trusted proxy: first forwarded hop"
        );
        assert_eq!(
            client_ip(&req, false),
            "unknown",
            "untrusted proxy: forwarded header ignored, no peer info"
        );
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                1234,
            ))));
        assert_eq!(
            client_ip(&req, false),
            "127.0.0.1",
            "untrusted proxy: socket peer address"
        );
    }

    /// A stream that emits one data frame and then stalls forever.
    struct SlowBody {
        sent: bool,
    }

    impl SlowBody {
        fn new() -> Self {
            Self { sent: false }
        }
    }

    impl hyper::body::Body for SlowBody {
        type Data = Bytes;
        type Error = Box<dyn std::error::Error + Send + Sync>;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
            if self.sent {
                Poll::Pending
            } else {
                self.sent = true;
                Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from_static(
                    b"one",
                )))))
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_aborts_a_stalled_stream() {
        let inner = http_body_util::combinators::BoxBody::new(SlowBody::new());
        let mut body = IdleTimeoutBody::new(
            inner,
            Duration::from_millis(50),
            "test-rid".to_owned(),
            Uuid::nil(),
        );

        // In-band frames pass through and restart the budget.
        let frame = http_body_util::BodyExt::frame(&mut body)
            .await
            .expect("in-band frame")
            .expect("in-band frame is ok");
        assert_eq!(frame.into_data().expect("data frame").as_ref(), b"one");

        // Stalling past the idle budget aborts the stream instead of
        // hanging forever.
        tokio::time::advance(Duration::from_millis(200)).await;
        let out = http_body_util::BodyExt::frame(&mut body).await;
        assert!(
            matches!(out, Some(Err(ref e)) if e.to_string().contains("idle")),
            "stalled stream must abort with an idle error, got {out:?}"
        );
    }

    #[test]
    fn select_route_skips_disabled_and_prefers_priority() {
        use crate::domain::model::{HttpMatch, HttpMethod, MatchRule, Route};
        let tenant = Uuid::from_u128(0x1234);
        let route = |enabled: bool, priority: u64, path: &str| RouteRecord {
            tenant_id: tenant,
            entity: Route {
                id: None,
                tags: Vec::new(),
                enabled,
                priority,
                upstream_id: "u".to_owned(),
                match_rule: MatchRule {
                    http: Some(HttpMatch {
                        methods: vec![HttpMethod::Get],
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        };
        let routes = vec![
            route(false, 10, "/v1"),
            route(true, 1, "/v1"),
            route(true, 5, "/v1"),
        ];
        let chain = vec![tenant];
        let picked = select_route(&routes, &chain, None, "GET", "/v1").expect("a route matches");
        assert_eq!(
            picked.entity.priority, 5,
            "disabled skipped, priority breaks ties"
        );
        assert!(select_route(&routes, &chain, None, "POST", "/v1").is_none());
    }
}
