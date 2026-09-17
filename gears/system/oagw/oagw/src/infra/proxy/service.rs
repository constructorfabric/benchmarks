//! The data plane service: proxy orchestration (DESIGN.md §3.5).
//!
//! One request flows
//!
//! ```text
//! resolve upstream → resolve route → plaintext policy → body validation
//!   → rate limit → circuit breaker → endpoint selection
//!   → plugin chain (auth → guards → transforms) → dial
//!   → plugin chain (guards/transforms on the response) → passthrough
//! ```
//!
//! and every failure of the gateway itself renders as an RFC 9457 problem
//! body with `X-OAGW-Error-Source: gateway` (see
//! [`crate::api::rest::error`]). Upstream requests are never retried and
//! upstream responses are never buffered.

use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use bytes::Bytes;
use toolkit_http::HttpError;
use toolkit_http::{HttpClient, HttpClientBuilder};
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::{Endpoint, HeadersConfig, RateLimitConfig};
use crate::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginError, RequestContext,
    ResponseContext, SecretResolver, TransformPlugin, error_kind_of,
};
use crate::domain::ratelimit::{RateLimitKey, RateLimitOutcome, RateLimiterRegistry};
use crate::domain::services::{ControlPlaneService, ResolvedProxyTarget};
use crate::infra::plugin::registry::PluginKind;
use crate::infra::proxy::builtins::BuiltinPlugins;
use crate::infra::proxy::circuit::{self, CircuitBreakers};
use crate::infra::proxy::cors;
use crate::infra::proxy::headers;
use crate::infra::proxy::streaming;
use crate::infra::proxy::target::{self, EndpointSelector};

/// A proxied request as handed to the data plane by the REST handler.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Upstream alias of the request (`/oagw/v1/proxy/{alias}/…`).
    pub alias: String,
    /// HTTP method.
    pub method: String,
    /// Path after the alias, unparsed.
    pub path: String,
    /// Query string as received (without the `?`).
    pub query: String,
    /// Inbound headers.
    pub headers: axum::http::HeaderMap,
    /// Request body.
    pub body: Bytes,
    /// The caller's pending protocol upgrade, when it asked for one.
    pub upgrade: Option<hyper::upgrade::OnUpgrade>,
}

impl ProxyRequest {
    /// The `X-OAGW-Target-Host` header value, when present.
    #[must_use]
    pub fn target_host(&self) -> Option<&str> {
        self.headers
            .get(headers::TARGET_HOST_HEADER)
            .and_then(|value| value.to_str().ok())
    }

    /// The first `X-Forwarded-For` entry, used by the `ip` rate-limit scope.
    #[must_use]
    pub fn client_ip(&self) -> Option<String> {
        self.headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
}

/// What the data plane produced for one proxied request.
#[derive(Debug)]
pub enum ProxyOutcome {
    /// The upstream answered; the response is passed through as-is.
    Upstream {
        /// Status the upstream answered with.
        status: u16,
        /// Response headers (hop-by-hop headers stripped, `headers.response`
        /// applied, `X-RateLimit-*` added when configured).
        headers: Vec<(String, String)>,
        /// Streaming response body.
        body: axum::body::Body,
    },
    /// The gateway itself failed; rendered as the gear's problem body.
    Gateway(DomainError),
    /// An answer the gateway produced itself — the CORS preflight (ADR-0004).
    Local {
        /// Status of the answer.
        status: u16,
        /// Headers of the answer.
        headers: Vec<(String, String)>,
        /// Body of the answer (empty).
        body: axum::body::Body,
    },
}

impl ProxyOutcome {
    /// Whether the outcome is a gateway error.
    #[must_use]
    pub const fn is_gateway_error(&self) -> bool {
        matches!(self, Self::Gateway(_))
    }
}

/// The data plane: executes proxied requests against the control plane's
/// configuration.
pub struct DataPlaneServiceImpl {
    config: OagwConfig,
    control_plane: ControlPlaneService,
    http: HttpClient,
    plugins: BuiltinPlugins,
    limiters: RateLimiterRegistry,
    circuits: CircuitBreakers,
    selector: EndpointSelector,
}

impl std::fmt::Debug for DataPlaneServiceImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneServiceImpl")
            .field("config", &self.config)
            .field("control_plane", &self.control_plane)
            .finish_non_exhaustive()
    }
}

impl DataPlaneServiceImpl {
    /// Composes the data plane from the configuration and the control plane.
    ///
    /// # Errors
    /// Returns [`ErrorKind::Internal`] when the outbound HTTP client cannot
    /// be built.
    pub fn new(
        config: OagwConfig,
        control_plane: ControlPlaneService,
        secrets: Arc<dyn SecretResolver>,
    ) -> Result<Self, DomainError> {
        let http = HttpClientBuilder::with_config(toolkit_http::HttpClientConfig::proxy())
            .build()
            .map_err(|error| {
                DomainError::new(
                    ErrorKind::Internal,
                    format!("outbound http client: {error}"),
                )
            })?;
        let plugins = BuiltinPlugins::new(
            secrets,
            config.token_cache_ttl(),
            config.token_cache_capacity as usize,
        );
        Ok(Self {
            config,
            control_plane,
            http,
            plugins,
            limiters: RateLimiterRegistry::new(),
            circuits: CircuitBreakers::new(),
            selector: EndpointSelector::default(),
        })
    }

    /// The control plane this data plane reads configuration from.
    #[must_use]
    pub const fn control_plane(&self) -> &ControlPlaneService {
        &self.control_plane
    }

    /// The circuit breakers of the data plane, for tests and diagnostics.
    #[must_use]
    pub const fn circuits(&self) -> &CircuitBreakers {
        &self.circuits
    }

    /// Executes one proxied request.
    ///
    /// # Errors
    /// Only fails when the configuration itself cannot be resolved; every
    /// request-level failure is returned as a [`ProxyOutcome::Gateway`].
    pub async fn proxy(
        &self,
        ctx: &SecurityContext,
        request: ProxyRequest,
    ) -> Result<ProxyOutcome, DomainError> {
        // A preflight carries no credentials and may address an alias the
        // caller cannot resolve, so it is answered before any of that.
        if let Some(headers) = self.preflight(&request) {
            return Ok(headers);
        }
        let tenant = ctx.subject_tenant_id();
        let suffix = crate::domain::matching::normalize_suffix(&request.path);
        let target = self
            .control_plane
            .resolve_proxy_target(ctx, tenant, &request.alias, &request.method, &suffix)
            .await?;
        self.validate_body(&request)?;
        let cors = self.cors_policy(&target);
        let rate_headers = self.enforce_rate_limit(ctx, &target, &request)?;
        let chain = self.bind(&target)?;
        let extra = self.check_cors(&request, cors.as_ref(), rate_headers)?;
        Ok(self.run(ctx, &target, &chain, &request, extra).await)
    }

    /// The locally answered preflight of a CORS request, if it is one.
    fn preflight(&self, request: &ProxyRequest) -> Option<ProxyOutcome> {
        if !cors::is_preflight(&request.method, &request.headers) {
            return None;
        }
        Some(ProxyOutcome::Local {
            status: StatusCode::NO_CONTENT.as_u16(),
            headers: cors::preflight_headers(&request.headers),
            body: axum::body::Body::empty(),
        })
    }

    /// The merged CORS policy of the resolved upstream and route (ADR-0004).
    fn cors_policy(
        &self,
        target: &ResolvedProxyTarget,
    ) -> Option<crate::domain::model::CorsConfig> {
        cors::effective_config(
            target.upstream.spec.cors.as_ref(),
            target.route.spec.cors.as_ref(),
        )
    }

    /// Validates the origin of an actual request and returns the headers the
    /// answer carries.
    ///
    /// A request without an `Origin` is not a browser request and bypasses
    /// the check entirely.
    ///
    /// # Errors
    /// Returns the ADR-0004 403 problems for a disallowed origin or method.
    fn check_cors(
        &self,
        request: &ProxyRequest,
        policy: Option<&crate::domain::model::CorsConfig>,
        mut forwarded: Vec<(String, String)>,
    ) -> Result<Vec<(String, String)>, DomainError> {
        let Some(policy) = policy else {
            return Ok(forwarded);
        };
        let Some(origin) = cors::origin_of(&request.headers) else {
            return Ok(forwarded);
        };
        cors::check_actual(policy, &origin, &request.method)?;
        forwarded.extend(cors::actual_headers(policy, &origin));
        Ok(forwarded)
    }

    // ------------------------------------------------------------- pre-flight

    /// Body validation (DESIGN.md §3.3 “Body Validation Rules”).
    ///
    /// # Errors
    /// Returns [`ErrorKind::ValidationError`] for a mismatching
    /// `Content-Length` or an unsupported `Transfer-Encoding`, and
    /// [`ErrorKind::PayloadTooLarge`] over the configured body limit.
    fn validate_body(&self, request: &ProxyRequest) -> Result<(), DomainError> {
        let limit = self.config.max_body_bytes as usize;
        if request.body.len() > limit {
            return Err(DomainError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "request body of {} bytes exceeds the {limit} byte limit",
                    request.body.len()
                ),
            ));
        }
        if let Some(declared) = request
            .headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
        {
            let declared = declared.trim().parse::<u64>().map_err(|_| {
                DomainError::new(
                    ErrorKind::ValidationError,
                    format!("`Content-Length` is not a valid integer: {declared:?}"),
                )
            })?;
            if declared != request.body.len() as u64 {
                return Err(DomainError::new(
                    ErrorKind::ValidationError,
                    format!(
                        "`Content-Length` {declared} does not match the body size of {} bytes",
                        request.body.len()
                    ),
                ));
            }
        }
        if let Some(encoding) = request
            .headers
            .get(axum::http::header::TRANSFER_ENCODING)
            .and_then(|value| value.to_str().ok())
        {
            let encoding = encoding.trim().to_ascii_lowercase();
            if encoding != "chunked" {
                return Err(DomainError::new(
                    ErrorKind::ValidationError,
                    format!(
                        "unsupported `Transfer-Encoding` {encoding:?}: only `chunked` is supported"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The declared body size, checked before the body is read.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PayloadTooLarge`] when the declared size exceeds
    /// the configured limit.
    pub fn check_declared_size(&self, content_length: Option<u64>) -> Result<(), DomainError> {
        let limit = self.config.max_body_bytes;
        if content_length.is_some_and(|length| length > limit) {
            let length = content_length.unwrap_or_default();
            return Err(DomainError::new(
                ErrorKind::PayloadTooLarge,
                format!("declared request body of {length} bytes exceeds the {limit} byte limit"),
            ));
        }
        Ok(())
    }

    /// Effective rate limit of a request: `min(upstream, route)` (ADR-0003).
    fn effective_rate_limit(&self, target: &ResolvedProxyTarget) -> Option<RateLimitConfig> {
        let upstream = target.upstream.spec.rate_limit.clone();
        let route = target.route.spec.rate_limit.clone();
        match (route, upstream) {
            (Some(route), Some(upstream)) => Some(crate::domain::ratelimit::merge_rate_limits(
                &route, &upstream,
            )),
            (Some(route), None) => Some(route),
            (None, Some(upstream)) => Some(upstream),
            (None, None) => None,
        }
    }

    /// Consumes a token for the request.
    ///
    /// # Errors
    /// Returns [`ErrorKind::RateLimitExceeded`] with `Retry-After` and the
    /// `X-RateLimit-*` headers when the bucket is exhausted.
    fn enforce_rate_limit(
        &self,
        ctx: &SecurityContext,
        target: &ResolvedProxyTarget,
        request: &ProxyRequest,
    ) -> Result<Vec<(String, String)>, DomainError> {
        let Some(config) = self.effective_rate_limit(target) else {
            return Ok(Vec::new());
        };
        let key = RateLimitKey {
            resource_id: target.route.id,
            scope_key: rate_scope_key(&config, ctx, request),
        };
        let (outcome, limit_headers) = self.limiters.check(&key, &config, Instant::now());
        let mut emitted: Vec<(String, String)> = Vec::new();
        if config.response_headers {
            emitted.push((
                "X-RateLimit-Limit".to_owned(),
                limit_headers.limit.to_string(),
            ));
            emitted.push((
                "X-RateLimit-Remaining".to_owned(),
                limit_headers.remaining.to_string(),
            ));
            emitted.push((
                "X-RateLimit-Reset".to_owned(),
                limit_headers.reset_seconds.to_string(),
            ));
        }
        if let RateLimitOutcome::Rejected {
            retry_after_seconds,
        } = outcome
        {
            let retry_after = retry_after_seconds.to_string();
            if !emitted.iter().any(|(name, _)| name == "Retry-After") {
                emitted.push(("Retry-After".to_owned(), retry_after.clone()));
            }
            // ADR-0003: the rejected answer carries the `X-RateLimit-*` and
            // `Retry-After` headers too.
            let mut error = DomainError::new(
                ErrorKind::RateLimitExceeded,
                format!(
                    "rate limit of {} requests per {}s exhausted for scope `{}`",
                    config.sustained.rate,
                    config.sustained.window.seconds(),
                    key.scope_key
                ),
            )
            .with_field(
                "retry_after_seconds",
                serde_json::json!(retry_after_seconds),
            )
            .with_field(
                "rate_limit",
                serde_json::json!({
                    "limit": limit_headers.limit,
                    "remaining": 0,
                    "reset_seconds": limit_headers.reset_seconds,
                }),
            );
            for (name, value) in emitted {
                error = error.with_header(name, value);
            }
            return Err(error);
        }
        Ok(emitted)
    }

    // ------------------------------------------------------------ plugin chain

    /// Resolves the plugin bindings of an upstream and its route.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PluginNotFound`] when a bound plugin cannot be
    /// resolved, [`ErrorKind::ValidationError`] when a binding is malformed.
    fn bind(&self, target: &ResolvedProxyTarget) -> Result<PluginChain, DomainError> {
        let upstream = &target.upstream;
        let mut chain = PluginChain::default();
        if let Some(auth) = &upstream.spec.auth {
            self.control_plane.resolve_plugin_ref(
                PluginKind::Auth,
                &auth.auth_type,
                upstream.tenant_id,
            )?;
            chain.auth = Some(self.plugins.build_auth(&auth.auth_type)?);
            chain.auth_config = auth.config.clone().unwrap_or(serde_json::Value::Null);
            chain.auth_reference = auth.auth_type.clone();
        }
        for (plugins, tenant) in [
            (upstream.spec.plugins.as_ref(), upstream.tenant_id),
            (target.route.spec.plugins.as_ref(), target.route.tenant_id),
        ] {
            let Some(plugins) = plugins else { continue };
            for item in &plugins.items {
                self.bind_plugin(&mut chain, item, tenant, plugins)?;
            }
        }
        chain.request_ops = request_ops(&upstream.spec.headers);
        chain.response_ops = response_ops(&upstream.spec.headers);
        Ok(chain)
    }

    /// Resolves one guard/transform binding into the chain.
    fn bind_plugin(
        &self,
        chain: &mut PluginChain,
        item: &crate::domain::model::PluginBinding,
        tenant_id: uuid::Uuid,
        plugins: &crate::domain::model::PluginsConfig,
    ) -> Result<(), DomainError> {
        let reference = item.reference();
        let config = plugins.config_of(reference);
        if let Ok(plugin) = self.plugins.build_guard(reference) {
            self.control_plane
                .resolve_plugin_ref(PluginKind::Guard, reference, tenant_id)?;
            chain.guards.push((reference.to_owned(), plugin));
            chain.configs.push((reference.to_owned(), config));
            return Ok(());
        }
        self.control_plane
            .resolve_plugin_ref(PluginKind::Transform, reference, tenant_id)?;
        let plugin = self.plugins.build_transform(reference)?;
        chain.transforms.push((reference.to_owned(), plugin));
        chain.configs.push((reference.to_owned(), config));
        Ok(())
    }

    // ------------------------------------------------------------------ dial

    /// Runs the plugin chain and dials the upstream.
    async fn run(
        &self,
        ctx: &SecurityContext,
        target: &ResolvedProxyTarget,
        chain: &PluginChain,
        request: &ProxyRequest,
        rate_headers: Vec<(String, String)>,
    ) -> ProxyOutcome {
        let upstream = &target.upstream;
        let slot = self.selector.next(upstream);
        let endpoint = match target::select_endpoint(upstream, request.target_host(), slot) {
            Ok(endpoint) => endpoint,
            Err(error) => return ProxyOutcome::Gateway(error),
        };
        if endpoint.scheme.as_str() == "http" && !self.config.allow_http_upstream {
            return ProxyOutcome::Gateway(
                DomainError::new(
                    ErrorKind::LinkUnavailable,
                    "plaintext `http` upstream endpoints are disabled by configuration",
                )
                .with_field(
                    "endpoint",
                    serde_json::json!(target::endpoint_base_url(endpoint)),
                ),
            );
        }
        if !target::ssrf_allows(&endpoint.host, self.config.ssrf_policy.enabled) {
            return ProxyOutcome::Gateway(DomainError::new(
                ErrorKind::LinkUnavailable,
                format!(
                    "endpoint host {} is refused by the SSRF policy",
                    endpoint.host
                ),
            ));
        }
        let host_key = target::endpoint_authority(endpoint);
        if !self.circuits.allows(&host_key, Instant::now()) {
            return ProxyOutcome::Gateway(circuit::open_error(&host_key));
        }
        let mut request_ctx = RequestContext {
            tenant_id: ctx.subject_tenant_id(),
            subject_id: ctx.subject_id(),
            method: request.method.clone(),
            path: target.upstream_path.clone(),
            query: request.query.clone(),
            headers: headers::outbound_request_headers(
                &request.headers,
                chain.request_ops.as_ref(),
            ),
            body: request.body.clone(),
            config: serde_json::Value::Null,
            attributes: Default::default(),
        };
        if let Err(error) = self
            .run_request_chain(&mut request_ctx, &request.headers, chain)
            .await
        {
            return ProxyOutcome::Gateway(error);
        }
        let url = format!(
            "{}{}",
            target::endpoint_base_url(endpoint),
            render_target(&target.upstream_path, &request.query)
        );
        let outbound_headers: Vec<(String, String)> = request_ctx
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .chain(std::iter::once(("host".to_owned(), host_key.clone())))
            .collect();
        // A protocol upgrade bypasses the plain proxy: the two ends negotiate
        // their own protocol over a spliced socket (PRD.md §5.4).
        if request.upgrade.is_some() {
            return self
                .upgrade(
                    endpoint,
                    &host_key,
                    &url,
                    outbound_headers,
                    request,
                    rate_headers,
                )
                .await;
        }
        let dial = self
            .dial(
                &request.method,
                &url,
                outbound_headers,
                request_ctx.body.clone(),
            )
            .await;
        match dial {
            Ok(response) => {
                self.circuits.record_success(&host_key);
                self.finish(chain, response, rate_headers).await
            }
            Err(error) => {
                self.circuits.record_failure(&host_key, Instant::now());
                let rendered = self.render_error(chain, error).await;
                match rendered {
                    ProxyOutcome::Gateway(mut error) => {
                        error
                            .fields
                            .push(("host", serde_json::json!(endpoint.host)));
                        error.fields.push((
                            "upstream_id",
                            serde_json::json!(target.upstream.id.to_string()),
                        ));
                        ProxyOutcome::Gateway(error)
                    }
                    outcome => outcome,
                }
            }
        }
    }

    /// Tunnels a protocol upgrade (PRD.md §5.4, §8).
    ///
    /// The caller is answered with the upstream's `101` headers; the two
    /// sockets are then spliced in the background, so frames flow in both
    /// directions without the gateway interpreting them.
    async fn upgrade(
        &self,
        endpoint: &Endpoint,
        host_key: &str,
        url: &str,
        outbound_headers: Vec<(String, String)>,
        request: &ProxyRequest,
        extra_headers: Vec<(String, String)>,
    ) -> ProxyOutcome {
        let Some(inbound) = request.upgrade.clone() else {
            return ProxyOutcome::Gateway(DomainError::new(
                ErrorKind::ProtocolError,
                "the request asked for no protocol upgrade",
            ));
        };
        let plan = streaming::TunnelRequest {
            method: request.method.clone(),
            url: url.to_owned(),
            headers: headers::restore_upgrade_headers(outbound_headers, &request.headers),
        };
        match streaming::open_tunnel(plan, self.config.proxy_timeout()).await {
            Ok(upstream) => {
                self.circuits.record_success(host_key);
                let mut headers = upstream.headers.clone();
                headers.extend(extra_headers);
                tokio::spawn(streaming::splice(upstream, inbound));
                ProxyOutcome::Upstream {
                    status: streaming::SWITCHING_PROTOCOLS,
                    headers,
                    body: axum::body::Body::empty(),
                }
            }
            Err(error) => {
                self.circuits.record_failure(host_key, Instant::now());
                let mut rendered = streaming::tunnel_error(error, host_key);
                rendered
                    .fields
                    .push(("host", serde_json::json!(endpoint.host)));
                ProxyOutcome::Gateway(rendered)
            }
        }
    }

    /// Executes the request-side plugin chain in ADR-0002 order.
    async fn run_request_chain(
        &self,
        ctx: &mut RequestContext,
        inbound: &axum::http::HeaderMap,
        chain: &PluginChain,
    ) -> Result<(), DomainError> {
        if let Some(plugin) = &chain.auth {
            ctx.config = chain.auth_config.clone();
            plugin
                .authenticate(ctx)
                .await
                .map_err(|error| plugin_error(&chain.auth_reference, error))?;
        }
        ctx.config = serde_json::Value::Null;
        for (reference, plugin) in &chain.guards {
            ctx.config = chain.config_of(reference);
            let guard_ctx = RequestContext {
                headers: inbound.clone(),
                config: ctx.config.clone(),
                ..ctx.clone()
            };
            match plugin.guard_request(&guard_ctx).await {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject {
                    status,
                    error_code,
                    detail,
                }) => {
                    return Err(guard_error(status, error_code, detail));
                }
                Err(error) => return Err(plugin_error(reference, error)),
            }
        }
        for (reference, plugin) in &chain.transforms {
            ctx.config = chain.config_of(reference);
            plugin
                .transform_request(ctx)
                .await
                .map_err(|error| plugin_error(reference, error))?;
        }
        ctx.config = serde_json::Value::Null;
        Ok(())
    }

    /// Dials the upstream once, with the configured timeout; never retries.
    async fn dial(
        &self,
        method: &str,
        url: &str,
        outbound_headers: Vec<(String, String)>,
        body: Bytes,
    ) -> Result<toolkit_http::HttpResponse, HttpError> {
        let parsed = method.parse::<axum::http::Method>().map_err(|_| {
            HttpError::Transport("request method is not a valid HTTP method".into())
        })?;
        let mut builder = match parsed {
            axum::http::Method::GET => self.http.get(url),
            axum::http::Method::POST => self.http.post(url),
            axum::http::Method::PUT => self.http.put(url),
            axum::http::Method::PATCH => self.http.patch(url),
            axum::http::Method::DELETE => self.http.delete(url),
            axum::http::Method::HEAD => self.http.head(url),
            axum::http::Method::OPTIONS => self.http.options(url),
            _ => self.http.post(url),
        };
        builder = builder.headers(outbound_headers).body_bytes(body);
        let timeout = self.config.proxy_timeout();
        let send = builder.send();
        if timeout.is_zero() {
            return send.await;
        }
        tokio::time::timeout(timeout, send)
            .await
            .unwrap_or_else(|_| Err(HttpError::Timeout(timeout)))
    }

    /// Renders a transport failure as a gateway error (DESIGN.md §3.3).
    async fn render_error(&self, chain: &PluginChain, error: HttpError) -> ProxyOutcome {
        let rendered = match error {
            HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => DomainError::new(
                ErrorKind::RequestTimeout,
                format!(
                    "the upstream did not answer within {:?}",
                    self.config.proxy_timeout()
                ),
            ),
            HttpError::InvalidScheme { scheme, .. } => DomainError::new(
                ErrorKind::LinkUnavailable,
                format!("the upstream scheme {scheme:?} is refused by the transport policy"),
            ),
            HttpError::BodyTooLarge { limit, actual } => DomainError::new(
                ErrorKind::PayloadTooLarge,
                format!("upstream response of {actual} bytes exceeds the {limit} byte limit"),
            ),
            other => DomainError::new(
                ErrorKind::ProtocolError,
                format!("the upstream link failed: {other}"),
            ),
        };
        self.apply_error_chain(chain, rendered).await
    }

    /// Applies the error-side plugin chain and returns the outcome.
    async fn apply_error_chain(&self, chain: &PluginChain, mut error: DomainError) -> ProxyOutcome {
        for (name, plugin) in &chain.transforms {
            let mut ctx = ErrorContext::from_error(&error);
            match plugin.transform_error(&mut ctx).await {
                Ok(()) => {
                    error.kind = ctx.kind;
                    error.detail = ctx.detail.clone();
                    error.headers = ctx.headers.clone();
                }
                Err(failure) => {
                    return ProxyOutcome::Gateway(plugin_error(name, failure));
                }
            }
        }
        ProxyOutcome::Gateway(error)
    }

    /// Runs the response-side plugin chain and builds the passthrough.
    async fn finish(
        &self,
        chain: &PluginChain,
        response: toolkit_http::HttpResponse,
        rate_headers: Vec<(String, String)>,
    ) -> ProxyOutcome {
        let status = response.status().as_u16();
        let mut response_headers = response.headers().clone();
        headers::strip_hop_by_hop(&mut response_headers);
        let guard_ctx = ResponseContext {
            status,
            headers: response_headers.clone(),
            body: Bytes::new(),
            config: serde_json::Value::Null,
        };
        for (name, plugin) in &chain.guards {
            match plugin.guard_response(&guard_ctx).await {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject {
                    status,
                    error_code,
                    detail,
                }) => {
                    return ProxyOutcome::Gateway(guard_error(status, error_code, detail));
                }
                Err(error) => return ProxyOutcome::Gateway(plugin_error(name, error)),
            }
        }
        let mut outbound =
            headers::outbound_response_headers(&response_headers, chain.response_ops.as_ref());
        for (name, plugin) in &chain.transforms {
            let mut transform_ctx = ResponseContext {
                status,
                headers: to_header_map(&outbound),
                body: Bytes::new(),
                config: chain.config_of(name),
            };
            if plugin.transform_response(&mut transform_ctx).await.is_ok() {
                outbound = from_header_map(&transform_ctx.headers);
            }
        }
        outbound.retain(|(name, _)| !name.eq_ignore_ascii_case("x-oagw-error-source"));
        for (name, value) in rate_headers {
            outbound.push((name, value));
        }
        let body = response.into_inner().into_body();
        ProxyOutcome::Upstream {
            status,
            headers: outbound,
            body: streaming::passthrough(body),
        }
    }
}

/// Resolved plugin bindings of one request.
#[derive(Default)]
struct PluginChain {
    auth: Option<Box<dyn AuthPlugin>>,
    auth_reference: String,
    auth_config: serde_json::Value,
    guards: Vec<(String, Box<dyn GuardPlugin>)>,
    transforms: Vec<(String, Box<dyn TransformPlugin>)>,
    configs: Vec<(String, serde_json::Value)>,
    request_ops: Option<crate::domain::model::RequestHeaderOps>,
    response_ops: Option<crate::domain::model::ResponseHeaderOps>,
}

impl PluginChain {
    /// The configuration bound to `reference`.
    fn config_of(&self, reference: &str) -> serde_json::Value {
        for (name, config) in &self.configs {
            if name == reference {
                return config.clone();
            }
        }
        serde_json::Value::Null
    }
}

fn plugin_error(reference: &str, error: PluginError) -> DomainError {
    DomainError::new(error_kind_of(error.status), error.detail)
        .with_field("error_code", serde_json::json!(error.error_code))
        .with_field("plugin", serde_json::json!(reference))
}

fn guard_error(status: u16, error_code: String, detail: String) -> DomainError {
    DomainError::new(error_kind_of(status), detail)
        .with_field("error_code", serde_json::json!(error_code))
}

fn request_ops(headers: &Option<HeadersConfig>) -> Option<crate::domain::model::RequestHeaderOps> {
    headers.as_ref().and_then(|config| config.request.clone())
}

fn response_ops(
    headers: &Option<HeadersConfig>,
) -> Option<crate::domain::model::ResponseHeaderOps> {
    headers.as_ref().and_then(|config| config.response.clone())
}

fn to_header_map(headers: &[(String, String)]) -> axum::http::HeaderMap {
    let mut map = axum::http::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::header::HeaderName::from_bytes(name.as_bytes()),
            axum::http::header::HeaderValue::from_str(value),
        ) {
            map.append(name, value);
        }
    }
    map
}

fn from_header_map(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

/// Renders the upstream URL path and query.
fn render_target(path: &str, query: &str) -> String {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    }
}

/// Scope key of a rate limit bucket (ADR-0003).
fn rate_scope_key(
    config: &RateLimitConfig,
    ctx: &SecurityContext,
    request: &ProxyRequest,
) -> String {
    use crate::domain::model::RateScope;
    match config.scope {
        RateScope::Global => "global".to_owned(),
        RateScope::Tenant => format!("tenant:{}", ctx.subject_tenant_id()),
        RateScope::User => format!("user:{}", ctx.subject_id()),
        RateScope::Ip => format!(
            "ip:{}",
            request.client_ip().unwrap_or_else(|| "unknown".to_owned())
        ),
        RateScope::Route => format!("route:{}", request.alias),
    }
}
