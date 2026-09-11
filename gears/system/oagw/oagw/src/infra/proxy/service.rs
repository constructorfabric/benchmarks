//! The proxy data plane implementation.
//!
//! [`GatewayService`] turns an incoming request into a [`ProxyPlan`], runs the
//! plugin chain, applies the route's header transforms, enforces the effective
//! rate limit and drives the outbound transport. Configuration is hierarchical:
//! the closest upstream wins, while an ancestor's enforced limits still apply.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use http::header::{CONNECTION, HOST, UPGRADE};
use http_body::Body as _;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::model::{
    AuthMethod, Endpoint, HeaderAction, HeaderTransform, Passthrough, RateLimit, Route, Upstream,
};
use crate::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, RequestContext, ResponseContext,
    TransformPlugin,
};
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::domain::services::control_plane::TenantHierarchy;
use crate::domain::services::data_plane::{
    CredentialResolver, DataPlaneService, DuplexStream, EndpointSelector, GatewayFailure,
    ProxyPlan, RateLimitOutcome, RateLimiter, UpstreamHandshake,
};
use crate::domain::services::routing::{RouteMatch, match_route};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::outbound::{
    OutboundTransport, PoolSelector, TARGET_HOST_HEADER, is_plaintext,
};

/// Header carrying the address the gateway saw the request from.
pub const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";

/// Response header naming the advertised rate limit.
pub const RATE_LIMIT_HEADER: &str = "x-ratelimit-limit";
/// Response header naming the tokens left in the bucket.
pub const RATE_LIMIT_REMAINING_HEADER: &str = "x-ratelimit-remaining";
/// Response header naming when the bucket is full again.
pub const RATE_LIMIT_RESET_HEADER: &str = "x-ratelimit-reset";

/// The header carrying the caller's correlation identifier.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Stamp the budget headers a rate-limited exchange advertises.
///
/// `Retry-After` itself is carried by the problem document's renderer.
fn rate_limit_headers(headers: &mut http::HeaderMap, outcome: &RateLimitOutcome) {
    headers.insert(
        RATE_LIMIT_HEADER,
        http::HeaderValue::from_str(&outcome.capacity.to_string())
            .unwrap_or(http::HeaderValue::from_static("0")),
    );
    headers.insert(
        RATE_LIMIT_REMAINING_HEADER,
        http::HeaderValue::from_str(&outcome.remaining.to_string())
            .unwrap_or(http::HeaderValue::from_static("0")),
    );
    headers.insert(
        RATE_LIMIT_RESET_HEADER,
        http::HeaderValue::from_str(&outcome.reset_at.to_string())
            .unwrap_or(http::HeaderValue::from_static("0")),
    );
}

/// Headers the gateway owns regardless of the passthrough setting: they
/// describe the body it forwards, which is not the caller's to withhold.
const GATEWAY_OWNED_HEADERS: &[&str] = &["content-length", "content-type"];

/// Headers a forwarded message must not carry (RFC 9110 §7.6.1).
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

/// The plugin chain built for one exchange.
#[derive(Default)]
struct Chain {
    auth: Vec<Arc<dyn AuthPlugin>>,
    guards: Vec<Arc<dyn GuardPlugin>>,
    transforms: Vec<Arc<dyn TransformPlugin>>,
    /// Credential injections declared by the upstream's `auth_methods`.
    credentials: Vec<CredentialInjection>,
}

/// One credential the upstream needs on every call.
#[derive(Debug, Clone)]
struct CredentialInjection {
    header_name: String,
    secret_ref: String,
}

impl CredentialInjection {
    /// The injection an `api_key` auth method declares.
    fn for_upstream(method: &AuthMethod) -> Option<Self> {
        match method {
            AuthMethod::ApiKey {
                secret_ref,
                header_name,
                ..
            } => Some(Self {
                header_name: header_name.clone(),
                secret_ref: secret_ref.clone(),
            }),
            // The client-credentials grant needs a token endpoint, not a
            // static key; it is not part of this build's data plane.
            AuthMethod::OAuth2ClientCredentials { .. } => None,
        }
    }

    /// Resolve and place the credential, never logging the value.
    async fn apply(
        &self,
        headers: &mut http::HeaderMap,
        ctx: &SecurityContext,
        resolver: &dyn CredentialResolver,
    ) -> Result<(), DomainError> {
        let value = resolver.resolve(ctx, &self.secret_ref).await?;
        let name = http::HeaderName::from_str(&self.header_name).map_err(|_| {
            DomainError::validation(format!(
                "'{}' is not a valid header name for an injected credential",
                self.header_name
            ))
        })?;
        let value = http::HeaderValue::from_str(&value).map_err(|_| {
            DomainError::validation("the resolved credential is not a valid header value")
        })?;
        // `insert` overwrites what the caller brought: the upstream's own
        // credential is not something a caller may pre-empt.
        headers.insert(name, value);
        Ok(())
    }
}

/// Executes proxied exchanges against the configured upstreams and routes.
pub struct GatewayService {
    routes: Arc<dyn RouteRepository>,
    upstreams: Arc<dyn UpstreamRepository>,
    hierarchy: Arc<dyn TenantHierarchy>,
    limiter: Arc<dyn RateLimiter>,
    resolver: Arc<dyn CredentialResolver>,
    auth: Arc<AuthPluginRegistry>,
    guards: Arc<GuardPluginRegistry>,
    transforms: Arc<TransformPluginRegistry>,
    transport: OutboundTransport,
    /// Largest request body the gateway forwards.
    max_body_bytes: u64,
}

impl std::fmt::Debug for GatewayService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayService").finish_non_exhaustive()
    }
}

impl GatewayService {
    /// Assemble the data plane over the given seams.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        routes: Arc<dyn RouteRepository>,
        upstreams: Arc<dyn UpstreamRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        limiter: Arc<dyn RateLimiter>,
        resolver: Arc<dyn CredentialResolver>,
        auth: Arc<AuthPluginRegistry>,
        guards: Arc<GuardPluginRegistry>,
        transforms: Arc<TransformPluginRegistry>,
        allow_http: bool,
        default_timeout_secs: u64,
        max_body_bytes: u64,
    ) -> Self {
        Self {
            routes,
            upstreams,
            hierarchy,
            limiter,
            resolver,
            auth,
            guards,
            transforms,
            transport: OutboundTransport::new(allow_http, default_timeout_secs),
            max_body_bytes,
        }
    }

    /// Replace the outbound transport, for tests that inject a fake.
    #[must_use]
    pub fn with_transport(mut self, transport: OutboundTransport) -> Self {
        self.transport = transport;
        self
    }

    /// The endpoint selector this service uses.
    #[must_use]
    pub fn selector() -> PoolSelector {
        PoolSelector::new()
    }

    async fn scope(&self, ctx: &SecurityContext) -> Result<Vec<uuid::Uuid>, DomainError> {
        let scope = self.hierarchy.scope(ctx).await?;
        if scope.is_empty() {
            return Err(DomainError::auth_failed("subject has no tenant"));
        }
        Ok(scope)
    }

    /// Match a route in the caller's scope, descendants first.
    async fn match_in_scope(
        &self,
        scope: &[uuid::Uuid],
        method: &str,
        path: &str,
    ) -> Result<RouteMatch, DomainError> {
        for tenant in scope {
            if let Ok(matched) = match_route(&self.routes, *tenant, method, path).await {
                return Ok(matched);
            }
        }
        Err(DomainError::route_not_found(format!(
            "no route matches {method} {path}"
        )))
    }

    /// Every enforced limit in scope, folded into one.
    async fn enforced_limits(
        &self,
        scope: &[uuid::Uuid],
    ) -> Result<Option<RateLimit>, DomainError> {
        let visible = self.upstreams.list_visible(scope).await?;
        let merged = visible
            .iter()
            .filter(|upstream| upstream.visible_to_descendants() && !upstream.allows_override())
            .filter_map(|upstream| upstream.rate_limit)
            .fold(None::<RateLimit>, |merged, limit| {
                RateLimit::merge_min(merged.as_ref(), Some(&limit))
            });
        Ok(merged)
    }

    /// Build the plugin chain for `route`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PluginNotFound`] when a bound plugin has no
    /// implementation in this build, and the plugin's own error when its
    /// configuration is rejected.
    fn build_chain(&self, route: &Route, upstream: &Upstream) -> Result<Chain, DomainError> {
        let mut chain = Chain {
            credentials: upstream
                .auth_methods
                .iter()
                .filter_map(CredentialInjection::for_upstream)
                .collect(),
            ..Chain::default()
        };
        for binding in &route.plugins {
            if !binding.enabled {
                continue;
            }
            let id = binding.plugin_id.as_str();
            if self.auth.has(id) {
                chain.auth.push(self.auth.create(id, &binding.config)?);
            } else if self.guards.has(id) {
                chain.guards.push(self.guards.create(id, &binding.config)?);
            } else if self.transforms.has(id) {
                chain
                    .transforms
                    .push(self.transforms.create(id, &binding.config)?);
            } else {
                return Err(DomainError::plugin_not_found(format!(
                    "plugin '{id}' is known to the catalog but has no implementation"
                )));
            }
        }
        Ok(chain)
    }

    /// Render a failure, letting the chain's `transform_error` add headers.
    async fn fail(&self, chain: &Chain, error: DomainError) -> GatewayFailure {
        let mut context = ErrorContext {
            error,
            headers: http::HeaderMap::new(),
        };
        for plugin in &chain.transforms {
            // A failing error transform never masks the original failure.
            plugin.transform_error(&mut context).await.ok();
        }
        GatewayFailure {
            error: context.error,
            headers: context.headers,
        }
    }

    /// Ask the bucket for one request's worth of tokens.
    ///
    /// The refusal carries the budget the caller has left in the same headers a
    /// successful exchange does: a client that throttles itself has to be able
    /// to read both answers the same way.
    async fn enforce_rate_limit(
        &self,
        ctx: &SecurityContext,
        plan: &ProxyPlan,
        client_ip: &str,
    ) -> Result<Option<RateLimitOutcome>, GatewayFailure> {
        let Some(key) = plan.limit_key(ctx, client_ip) else {
            return Ok(None);
        };
        let Some(limit) = plan.rate_limit else {
            return Ok(None);
        };
        let outcome = self.limiter.acquire(&key, &limit, limit.cost.max(1)).await;
        if !outcome.allowed {
            let mut headers = http::HeaderMap::new();
            if limit.response_headers {
                rate_limit_headers(&mut headers, &outcome);
            }
            return Err(GatewayFailure {
                error: DomainError::rate_limit_exceeded(
                    "rate limit exhausted for this scope",
                    outcome.retry_after_secs,
                ),
                headers,
            });
        }
        Ok(Some(outcome))
    }

    /// The address the gateway saw the request from.
    #[must_use]
    pub fn client_ip(headers: &http::HeaderMap) -> String {
        headers
            .get(FORWARDED_FOR_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map_or_else(|| "unknown".to_owned(), |value| value.trim().to_owned())
    }

    /// Whether a request asks for a protocol upgrade: `Upgrade: …websocket`
    /// plus the `Connection: upgrade` token, both required.
    #[must_use]
    pub fn is_upgrade(headers: &http::HeaderMap) -> bool {
        asks_upgrade(headers, &UPGRADE) && asks_upgrade(headers, &CONNECTION)
    }

    /// The authority the upstream is dialled on.
    #[must_use]
    pub fn endpoint_authority(endpoint: &Endpoint) -> String {
        let host = endpoint.host.trim();
        if host.starts_with('[') {
            return format!("{host}:{}", endpoint.port_or_default());
        }
        if host.contains(':') {
            // An unbracketed IPv6 literal needs brackets to be a valid
            // authority.
            return format!("[{host}]:{}", endpoint.port_or_default());
        }
        endpoint.host_authority()
    }

    /// The scheme spoken to the endpoint.
    #[must_use]
    pub const fn outbound_scheme(endpoint: &Endpoint) -> &'static str {
        if is_plaintext(endpoint.scheme) {
            "http"
        } else {
            "https"
        }
    }

    /// The URI forwarded to the endpoint.
    ///
    /// Origin-form, not absolute-form: hyper writes onto the wire exactly what
    /// the request URI carries, and an upstream that matches on its path —
    /// which is nearly all of them — would see `http://host/p` where it wants
    /// `/p`. The dial and the `Host` header carry the authority, so the URI
    /// does not have to.
    fn target_uri(path_and_query: &str) -> Result<http::Uri, DomainError> {
        http::Uri::builder()
            .path_and_query(path_and_query)
            .build()
            .map_err(|err| {
                DomainError::validation(format!("forwarded target is not a valid URI: {err}"))
            })
    }

    async fn apply_transforms(
        &self,
        headers: &mut http::HeaderMap,
        transforms: &[HeaderTransform],
        ctx: &SecurityContext,
    ) -> Result<(), DomainError> {
        for transform in transforms {
            let Ok(name) = http::HeaderName::from_bytes(transform.name.as_bytes()) else {
                return Err(DomainError::validation(format!(
                    "'{}' is not a valid header name",
                    transform.name
                )));
            };
            match transform.action {
                HeaderAction::Remove => {
                    headers.remove(&name);
                }
                HeaderAction::Replace if headers.contains_key(&name) => {
                    self.insert_resolved(headers, &name, transform, ctx).await?;
                }
                // A replace has nothing to say about a header nobody sent:
                // inventing one is what `set` is for.
                HeaderAction::Replace => {}
                HeaderAction::Set => {
                    self.insert_resolved(headers, &name, transform, ctx).await?;
                }
            }
        }
        Ok(())
    }

    async fn insert_resolved(
        &self,
        headers: &mut http::HeaderMap,
        name: &http::HeaderName,
        transform: &HeaderTransform,
        ctx: &SecurityContext,
    ) -> Result<(), DomainError> {
        let value = if transform.value_ref.is_empty() {
            transform.value.clone()
        } else {
            self.resolver.resolve(ctx, &transform.value_ref).await?
        };
        let rendered = http::HeaderValue::from_str(&value).map_err(|_| {
            DomainError::validation(format!("'{}' is not a valid header value", transform.name))
        })?;
        headers.insert(name, rendered);
        Ok(())
    }

    /// Drop the caller's own headers the route's passthrough setting does not
    /// forward.
    ///
    /// The gate runs last, after the chain and the transforms have said their
    /// piece, so what a plugin injected and what a transform wrote is never its
    /// business — only what the caller brought with it is. `Content-Length` and
    /// `Content-Type` stay as well: they describe the body the gateway forwards
    /// verbatim, and a body without them is not the message the caller sent.
    fn apply_passthrough(
        headers: &mut http::HeaderMap,
        upstream: &Upstream,
        route: &Route,
        injected: &[String],
    ) {
        if matches!(route.passthrough, Passthrough::All) {
            return;
        }
        // The credentials the upstream itself asked for are the gateway's own
        // contribution, whatever header they land in.
        let credentials: Vec<&str> = upstream
            .auth_methods
            .iter()
            .map(AuthMethod::header_name)
            .collect();
        let configured: Vec<&str> = route
            .request_headers
            .iter()
            .filter(|transform| !matches!(transform.action, HeaderAction::Remove))
            .map(|transform| transform.name.as_str())
            .collect();
        for name in headers.keys().cloned().collect::<Vec<_>>() {
            let kept = GATEWAY_OWNED_HEADERS.contains(&name.as_str())
                || credentials
                    .iter()
                    .any(|kept| kept.eq_ignore_ascii_case(name.as_str()))
                || configured
                    .iter()
                    .any(|kept| kept.eq_ignore_ascii_case(name.as_str()))
                || injected
                    .iter()
                    .any(|kept| kept.eq_ignore_ascii_case(name.as_str()))
                || route
                    .passthrough
                    .forwards(&route.passthrough_allowlist, name.as_str());
            if !kept {
                headers.remove(&name);
            }
        }
    }

    /// Strip the hop-by-hop headers a forwarded message must not carry.
    pub fn strip_hop_by_hop(headers: &mut http::HeaderMap) {
        for name in HOP_BY_HOP {
            headers.remove(*name);
        }
        let named: Vec<String> = headers
            .get(CONNECTION)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(',')
                    .map(|entry| entry.trim().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        headers.remove(CONNECTION);
        for name in &named {
            headers.remove(name);
        }
    }

    /// Drop the gateway's own control headers before a request is forwarded.
    ///
    /// `X-OAGW-Target-Host` is read during routing and then stripped: the
    /// upstream it names is the gateway's business, and a header that leaks
    /// through would let a caller steer a second hop.
    pub fn strip_gateway_headers(headers: &mut http::HeaderMap) {
        headers.remove(TARGET_HOST_HEADER);
    }
}

#[async_trait]
impl DataPlaneService for GatewayService {
    async fn plan(
        &self,
        ctx: &SecurityContext,
        method: &str,
        alias: &str,
        path: &str,
        target_host: Option<&str>,
    ) -> Result<ProxyPlan, DomainError> {
        let scope = self.scope(ctx).await?;
        let upstream = self
            .upstreams
            .get_by_alias(&scope, alias)
            .await?
            .ok_or_else(|| {
                DomainError::route_not_found(format!(
                    "no upstream '{alias}' is visible to this tenant"
                ))
            })?;
        if !upstream.accepts_traffic() {
            // The pool is switched off — by its owner or by an ancestor, which
            // reaches the caller either way. The answer is the gateway's, and
            // the upstream is never dialled (PRD FR: enabled).
            return Err(DomainError::link_unavailable(format!(
                "upstream '{alias}' is disabled"
            )));
        }
        let matched = self.match_in_scope(&scope, method, path).await?;
        if !matched.route.path_suffix_mode.allows_suffix() {
            // The route's path is the whole address: anything beyond it would
            // aim the call at a resource the operator never mapped.
            let suffix = path
                .strip_prefix(&matched.matched_prefix)
                .unwrap_or(path)
                .trim_start_matches('/');
            if !suffix.is_empty() {
                return Err(DomainError::validation(format!(
                    "route '{}' does not accept a path suffix, and '{}' was requested",
                    matched.route.path, path
                )));
            }
        }
        let endpoint = Self::selector().select(&upstream, target_host)?;
        let forward_path = matched.route.forward_path(&matched.matched_prefix, path);
        let configured = RateLimit::merge_min(
            upstream.rate_limit.as_ref(),
            matched.route.rate_limit.as_ref(),
        );
        let enforced = self.enforced_limits(&scope).await?;
        Ok(ProxyPlan {
            route: matched.route,
            upstream,
            endpoint,
            forward_path,
            rate_limit: RateLimit::merge_min(configured.as_ref(), enforced.as_ref()),
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn forward(
        &self,
        ctx: &SecurityContext,
        plan: &ProxyPlan,
        request: http::Request<axum::body::Body>,
    ) -> Result<http::Response<axum::body::Body>, GatewayFailure> {
        let (mut parts, body) = request.into_parts();
        let client_ip = Self::client_ip(&parts.headers);
        let outcome = self.enforce_rate_limit(ctx, plan, &client_ip).await?;
        // The body is policed before the exchange goes anywhere: a payload the
        // gateway cannot account for is one it refuses to forward, and the
        // refusal costs nothing (DESIGN: body validation rules).
        check_body(
            &parts.method,
            &parts.headers,
            body.size_hint().exact(),
            Some(self.max_body_bytes),
        )?;

        let chain = self
            .build_chain(&plan.route, &plan.upstream)
            .map_err(GatewayFailure::new)?;
        let query = parts.uri.query().unwrap_or_default().to_owned();
        let body_present = !matches!(
            parts.method,
            http::Method::GET | http::Method::HEAD | http::Method::OPTIONS
        );
        let mut request_ctx = RequestContext {
            method: parts.method.as_str().to_owned(),
            path: plan.forward_path.clone(),
            query,
            headers: std::mem::take(&mut parts.headers),
            body_present,
            security_context: ctx.clone(),
            tenant_scope: vec![plan.upstream.tenant_id],
            injected_headers: Vec::new(),
            attributes: std::collections::HashMap::new(),
        };

        for plugin in &chain.auth {
            if let Err(err) = plugin.authenticate(&mut request_ctx).await {
                return Err(self.fail(&chain, err).await);
            }
        }
        // A plugin that needs a credential declares it as an attribute rather
        // than resolving it itself, because the resolver is not handed to
        // plugins. Resolving is the data plane's job, and a reference that
        // names nothing aborts the exchange before a byte leaves the gateway.
        if let Err(err) =
            inject_plugin_credentials(&mut request_ctx, ctx, self.resolver.as_ref()).await
        {
            return Err(self.fail(&chain, err).await);
        }
        // The upstream's own credentials come before anything the caller can
        // influence: a missing secret aborts the exchange here, and the request
        // never leaves the gateway.
        for injection in &chain.credentials {
            if let Err(err) = injection
                .apply(&mut request_ctx.headers, ctx, self.resolver.as_ref())
                .await
            {
                return Err(self.fail(&chain, err).await);
            }
        }
        if let Some(rejection) = guard_request(&chain.guards, &request_ctx)
            .await
            .into_iter()
            .next()
        {
            return Err(self.fail(&chain, rejection).await);
        }
        for plugin in &chain.transforms {
            if let Err(err) = plugin.transform_request(&mut request_ctx).await {
                return Err(self.fail(&chain, err).await);
            }
        }
        if let Err(err) = self
            .apply_transforms(&mut request_ctx.headers, &plan.route.request_headers, ctx)
            .await
        {
            return Err(self.fail(&chain, err).await);
        }

        let injected = request_ctx.injected_headers.clone();
        Self::apply_passthrough(
            &mut request_ctx.headers,
            &plan.upstream,
            &plan.route,
            &injected,
        );
        Self::strip_hop_by_hop(&mut request_ctx.headers);
        Self::strip_gateway_headers(&mut request_ctx.headers);
        let authority = Self::endpoint_authority(&plan.endpoint);
        if plan.route.preserve_host {
            if !request_ctx.headers.contains_key(HOST)
                && let Ok(value) = http::HeaderValue::from_str(&authority)
            {
                request_ctx.headers.insert(HOST, value);
            }
        } else {
            let host = http::HeaderValue::from_str(&authority).map_err(|_| {
                GatewayFailure::from(DomainError::validation(
                    "the endpoint authority is not a valid Host header",
                ))
            })?;
            request_ctx.headers.insert(HOST, host);
        }

        let path_and_query = if request_ctx.query.is_empty() {
            plan.forward_path.clone()
        } else {
            format!("{}?{}", plan.forward_path, request_ctx.query)
        };
        let uri = Self::target_uri(&path_and_query).map_err(GatewayFailure::new)?;

        let mut builder = http::Request::builder()
            .method(parts.method.clone())
            .uri(uri);
        for (name, value) in &request_ctx.headers {
            builder = builder.header(name, value);
        }
        let outbound = builder.body(body).map_err(|err| {
            GatewayFailure::from(DomainError::protocol(format!(
                "forwarded request could not be built: {err}"
            )))
        })?;

        let timeout = self.transport.timeout_for(plan.route.timeout_secs);
        let response = match self.transport.send(&plan.endpoint, outbound, timeout).await {
            Ok(response) => response,
            Err(err) => return Err(self.fail(&chain, err).await),
        };

        self.finish_response(ctx, plan, &chain, outcome, response, &request_ctx.headers)
            .await
    }

    async fn open_tunnel(
        &self,
        _ctx: &SecurityContext,
        plan: &ProxyPlan,
        request: &http::request::Parts,
    ) -> Result<UpstreamHandshake, DomainError> {
        let mut head = request.clone();
        let mut headers = std::mem::take(&mut head.headers);
        // The caller's intent is read from its own head: stripping the
        // hop-by-hop set removes `Connection` itself, so it has to be read
        // first.
        let wanted = asks_upgrade(&headers, &UPGRADE) && asks_upgrade(&headers, &CONNECTION);
        Self::strip_hop_by_hop(&mut headers);
        Self::strip_gateway_headers(&mut headers);
        let authority = Self::endpoint_authority(&plan.endpoint);
        let host = http::HeaderValue::from_str(&authority)
            .map_err(|_| DomainError::validation("the endpoint authority is not a valid host"))?;
        headers.insert(HOST, host);
        head.headers = headers;
        head.version = http::Version::HTTP_11;
        head.uri = Self::target_uri(&plan.forward_path)?;
        if !wanted {
            return Err(DomainError::validation(
                "request does not ask for a protocol upgrade",
            ));
        }
        // An upstream that authenticates its WebSocket dials is authenticated
        // on the upgrade head, the only message it will see.
        let chain = self.build_chain(&plan.route, &plan.upstream)?;
        for injection in &chain.credentials {
            injection
                .apply(&mut head.headers, _ctx, self.resolver.as_ref())
                .await?;
        }
        // A route-bound auth plugin speaks on the head too, the only message the
        // upstream will see, so what it declares is resolved here as well.
        let mut tunnel_ctx = RequestContext {
            method: head.method.as_str().to_owned(),
            path: plan.forward_path.clone(),
            query: String::new(),
            headers: std::mem::take(&mut head.headers),
            body_present: false,
            security_context: _ctx.clone(),
            tenant_scope: vec![plan.upstream.tenant_id],
            injected_headers: Vec::new(),
            attributes: std::collections::HashMap::new(),
        };
        for plugin in &chain.auth {
            plugin.authenticate(&mut tunnel_ctx).await?;
        }
        inject_plugin_credentials(&mut tunnel_ctx, _ctx, self.resolver.as_ref()).await?;
        head.headers = std::mem::take(&mut tunnel_ctx.headers);
        // The stripped pair governs the *caller's* hop; the upstream leg needs
        // its own, so the tunnel is dialled with it re-added.
        head.headers
            .insert(CONNECTION, http::HeaderValue::from_static("Upgrade"));
        head.headers
            .insert(UPGRADE, http::HeaderValue::from_static("websocket"));
        let timeout = self.transport.timeout_for(plan.route.timeout_secs);
        let handshake = self
            .transport
            .dial_upgrade(&plan.endpoint, &head, timeout)
            .await?;
        Ok(UpstreamHandshake {
            status: handshake.status,
            headers: handshake.headers,
            stream: handshake.stream,
        })
    }

    async fn bridge(
        &self,
        mut client: DuplexStream,
        mut upstream: DuplexStream,
    ) -> Result<(), DomainError> {
        self.transport.bridge(&mut client, &mut upstream).await
    }
}

async fn guard_request(guards: &[Arc<dyn GuardPlugin>], ctx: &RequestContext) -> Vec<DomainError> {
    let mut rejections = Vec::new();
    for plugin in guards {
        match plugin.guard_request(ctx).await {
            Ok(GuardDecision::Allow) => {}
            Ok(rejection) => {
                if let Some(error) = rejection.rejection().cloned() {
                    rejections.push(error);
                }
            }
            Err(error) => rejections.push(error),
        }
    }
    rejections
}

/// Resolve and inject the credential a plugin asked for, if it asked for one.
///
/// The plugin records what it needs as attributes because it cannot await a
/// resolver it was never handed; the data plane owns the store call and places
/// the value where the plugin said to. A reference that names nothing aborts
/// the exchange, and the value is placed straight into the outbound header map
/// — never logged, never returned to a caller.
///
/// # Errors
/// Returns [`ErrorKind::SecretNotFound`] when the reference resolves to
/// nothing, and whatever the resolver itself fails with.
async fn inject_plugin_credentials(
    ctx: &mut RequestContext,
    subject: &SecurityContext,
    resolver: &dyn CredentialResolver,
) -> Result<(), DomainError> {
    let Some(secret_ref) = ctx
        .attributes
        .get(crate::infra::plugin::apikey_auth::SECRET_REF_ATTRIBUTE)
    else {
        return Ok(());
    };
    let header_name = ctx
        .attributes
        .get(crate::infra::plugin::apikey_auth::HEADER_NAME_ATTRIBUTE)
        .map_or("Authorization", String::as_str)
        .to_owned();
    let value = resolver.resolve(subject, secret_ref).await?;
    ctx.set_header(&header_name, &value);
    Ok(())
}

/// Check a request body against the gateway's body rules.
///
/// The rules are the ones DESIGN spells out for every request, with no
/// configuration required: a declared `Content-Length` must agree with the
/// body, `Content-Length` and `Transfer-Encoding` may not both be present, an
/// encoding other than `chunked` is refused, and the payload may not exceed
/// the limit. A body a gateway cannot account for is one it does not forward.
///
/// # Errors
/// Returns [`ErrorKind::PayloadTooLarge`] beyond the limit and
/// [`ErrorKind::Validation`] for a request whose framing disagrees with
/// itself.
fn check_body(
    method: &http::Method,
    headers: &http::HeaderMap,
    actual_len: Option<u64>,
    limit: Option<u64>,
) -> Result<(), DomainError> {
    let has_length = headers.contains_key(http::header::CONTENT_LENGTH);
    let transfer_encoding = headers
        .get(http::header::TRANSFER_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase);
    if has_length && transfer_encoding.is_some() {
        // Two authoritative lengths is the shape request smuggling takes, so
        // the request is refused rather than resolved by preference.
        return Err(DomainError::validation(
            "a request may not carry both Content-Length and Transfer-Encoding",
        ));
    }
    if let Some(encoding) = transfer_encoding
        && encoding
            .split(',')
            .any(|token| !token.trim().is_empty() && token.trim() != "chunked")
    {
        return Err(DomainError::validation(format!(
            "'{encoding}' is not a supported Transfer-Encoding; only 'chunked' is"
        )));
    }
    let declared = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(declared) = declared
        && let Some(actual_len) = actual_len
        && declared != actual_len
    {
        return Err(DomainError::validation(format!(
            "Content-Length declares {declared} bytes but the request carries {actual_len}"
        )));
    }
    if let Some(limit) = limit {
        let size = declared.or(actual_len).unwrap_or_default();
        if size > limit {
            return Err(DomainError::payload_too_large(format!(
                "the request body is {size} bytes and the limit is {limit}"
            )));
        }
    }
    // A method that never carries a body cannot exceed a limit, whatever its
    // framing headers claim.
    if matches!(
        *method,
        http::Method::GET | http::Method::HEAD | http::Method::OPTIONS
    ) {
        return Ok(());
    }
    Ok(())
}

fn asks_upgrade(headers: &http::HeaderMap, name: &http::HeaderName) -> bool {
    // `Upgrade` names the protocol (`websocket`), `Connection` carries the
    // `upgrade` token; only the token is spelled out.
    if *name == CONNECTION {
        return headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .to_ascii_lowercase()
                    .split(',')
                    .any(|token| token.trim() == "upgrade")
            });
    }
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

#[allow(clippy::too_many_arguments)]
impl GatewayService {
    async fn finish_response(
        &self,
        ctx: &SecurityContext,
        plan: &ProxyPlan,
        chain: &Chain,
        outcome: Option<RateLimitOutcome>,
        response: http::Response<axum::body::Body>,
        request_headers: &http::HeaderMap,
    ) -> Result<http::Response<axum::body::Body>, GatewayFailure> {
        let (mut parts, body) = response.into_parts();
        let mut response_ctx = ResponseContext {
            status: parts.status,
            headers: std::mem::take(&mut parts.headers),
            injected_headers: Vec::new(),
        };
        // The identifier the request was given is echoed on the answer: a
        // caller that correlates the two legs of an exchange reads them
        // together, whether or not the upstream echoed it back.
        if let Some(identifier) = request_headers.get(REQUEST_ID_HEADER) {
            response_ctx
                .headers
                .insert(REQUEST_ID_HEADER, identifier.clone());
        }
        for plugin in &chain.transforms {
            if let Err(err) = plugin.transform_response(&mut response_ctx).await {
                return Err(self.fail(chain, err).await);
            }
        }
        for plugin in &chain.guards {
            match plugin.guard_response(&response_ctx).await {
                Ok(GuardDecision::Allow) => {}
                Ok(rejection) => {
                    let error = rejection
                        .rejection()
                        .cloned()
                        .unwrap_or_else(|| DomainError::downstream("response rejected"));
                    return Err(self.fail(chain, error).await);
                }
                Err(err) => return Err(self.fail(chain, err).await),
            }
        }
        if let Err(err) = self
            .apply_transforms(&mut response_ctx.headers, &plan.route.response_headers, ctx)
            .await
        {
            return Err(self.fail(chain, err).await);
        }

        Self::strip_hop_by_hop(&mut response_ctx.headers);
        if let (Some(outcome), Some(limit)) = (outcome, plan.rate_limit.as_ref())
            && limit.response_headers
        {
            rate_limit_headers(&mut response_ctx.headers, &outcome);
        }
        response_ctx.headers.insert(
            crate::api::rest::error::ERROR_SOURCE_HEADER,
            http::HeaderValue::from_static(crate::api::rest::error::ERROR_SOURCE_UPSTREAM),
        );

        parts.status = response_ctx.status;
        parts.headers = response_ctx.headers;
        Ok(http::Response::from_parts(parts, body))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod target_host_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::model::Scheme;

    fn endpoint(host: &str, port: Option<u16>, scheme: Scheme) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
            ..Endpoint::default()
        }
    }

    #[test]
    fn an_endpoint_authority_brackets_ipv6() {
        assert_eq!(
            GatewayService::endpoint_authority(&endpoint("::1", Some(8080), Scheme::Https)),
            "[::1]:8080"
        );
        assert_eq!(
            GatewayService::endpoint_authority(&endpoint("[::1]", Some(8080), Scheme::Http)),
            "[::1]:8080"
        );
        assert_eq!(
            GatewayService::endpoint_authority(&endpoint(
                "api.partner.com",
                Some(443),
                Scheme::Https
            )),
            "api.partner.com"
        );
        assert_eq!(
            GatewayService::endpoint_authority(&endpoint(
                "api.partner.com",
                Some(8443),
                Scheme::Https
            )),
            "api.partner.com:8443"
        );
    }

    #[test]
    fn plaintext_endpoints_speak_http_and_the_rest_tls() {
        assert_eq!(
            GatewayService::outbound_scheme(&endpoint("h", None, Scheme::Http)),
            "http"
        );
        assert_eq!(
            GatewayService::outbound_scheme(&endpoint("h", None, Scheme::Https)),
            "https"
        );
        assert_eq!(
            GatewayService::outbound_scheme(&endpoint("h", None, Scheme::Wss)),
            "https"
        );
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            CONNECTION,
            http::HeaderValue::from_static("keep-alive, X-Custom"),
        );
        headers.insert(
            http::header::HeaderName::from_static("keep-alive"),
            http::HeaderValue::from_static("timeout=5"),
        );
        headers.insert(
            http::header::HeaderName::from_static("x-custom"),
            http::HeaderValue::from_static("kept"),
        );
        GatewayService::strip_hop_by_hop(&mut headers);
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("connection").is_none());
        assert!(headers.get("x-custom").is_some());
    }

    #[test]
    fn the_client_ip_prefers_the_forwarded_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::HeaderName::from_static("x-forwarded-for"),
            http::HeaderValue::from_static("203.0.113.9, 10.0.0.1"),
        );
        assert_eq!(GatewayService::client_ip(&headers), "203.0.113.9");
        assert_eq!(
            GatewayService::client_ip(&http::HeaderMap::new()),
            "unknown"
        );
    }

    #[test]
    fn an_upgraded_request_must_ask_for_it() {
        let mut headers = http::HeaderMap::new();
        headers.insert(UPGRADE, http::HeaderValue::from_static("websocket"));
        assert!(!asks_upgrade(&headers, &CONNECTION));
        headers.insert(CONNECTION, http::HeaderValue::from_static("Upgrade"));
        assert!(asks_upgrade(&headers, &CONNECTION));
        assert!(asks_upgrade(&headers, &UPGRADE));
    }
}
