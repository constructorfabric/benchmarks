//! The forwarding pipeline (`DESIGN` §3.5 "Proxy Request Flow", `ADR`-0002).
//!
//! One call carries a proxied request through the whole path:
//!
//! ```text
//! resolve alias → select route → rate limit → CORS → select endpoint
//!   → auth → guards → transform(request) → upstream exchange
//!   → guard(response) → transform(response) → response header rules
//! ```
//!
//! Every failure is a [`DomainError`]; the transport layer maps it onto the
//! problem of `DESIGN` §3.3 and stamps `X-OAGW-Error-Source: gateway`, while a
//! response the upstream produced keeps `X-OAGW-Error-Source: upstream`.

use std::collections::BTreeMap;
use std::sync::Arc;

use dashmap::DashMap;
use http::HeaderMap;

use crate::domain::dto::ProxyContext;
use crate::domain::error::DomainError;
use crate::domain::model::{GUARD_PLUGIN_TYPE, Route, TRANSFORM_PLUGIN_TYPE, Upstream};
use crate::domain::plugin::{AuthOutcome, GuardDecision, ResponseContext};
use crate::domain::repo::{RouteRepository, UpstreamRepository};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::cors::CorsPolicy;
use crate::infra::proxy::endpoint::{EndpointSelector, SelectedEndpoint};
use crate::infra::proxy::headers::{outbound_request_headers, outbound_response_headers};
use crate::infra::proxy::policy::ProxyPolicy;
use crate::infra::proxy::ratelimit::{RateDecision, RateLimiter};
use crate::infra::proxy::resolver::{AliasResolver, ResolvedUpstream, TenantHierarchy};
use crate::infra::proxy::route_match::{SelectedRoute, select_route};
use crate::infra::proxy::ssrf::SsrfGuard;
use crate::infra::proxy::transport::{TunnelStream, UpstreamTransport, outbound_uri};

/// A request handed to the data plane.
pub struct ForwardRequest {
    /// Domain view of the call, mutated by the plugin chain.
    pub context: ProxyContext,
    /// Wire method.
    pub method: http::Method,
    /// `X-OAGW-Target-Host`, extracted from the inbound headers by the caller.
    pub target_host: Option<String>,
    /// The caller's body, streamed.
    pub body: axum::body::Body,
    /// `true` when the caller asked for a protocol upgrade.
    pub upgrade: bool,
}

/// What the data plane answers with.
pub enum ForwardOutcome {
    /// The upstream's response, body still streaming.
    Response(http::Response<axum::body::Body>),
    /// A WebSocket tunnel: the upstream `101` headers and its raw stream.
    Tunnel {
        /// Upstream handshake headers, forwarded to the caller.
        parts: http::response::Parts,
        /// The upstream side of the tunnel.
        stream: TunnelStream,
    },
}

/// One limiter per rate-limit configuration, created on first use and rebuilt
/// when the configuration that produced it is replaced.
type Limiters = DashMap<uuid::Uuid, (crate::domain::model::RateLimitConfig, RateLimiter)>;

/// The data plane: everything a proxied request needs to reach an upstream.
#[derive(Clone)]
pub struct ProxyEngine {
    resolver: AliasResolver,
    routes: Arc<dyn RouteRepository>,
    endpoints: EndpointSelector,
    ssrf: SsrfGuard,
    auth: AuthPluginRegistry,
    policy: ProxyPolicy,
    transport: UpstreamTransport,
    limiters: Arc<Limiters>,
}

impl std::fmt::Debug for ProxyEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyEngine")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl ProxyEngine {
    /// Assemble the data plane over its ports.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        auth: AuthPluginRegistry,
        ssrf: SsrfGuard,
        policy: ProxyPolicy,
        transport: UpstreamTransport,
    ) -> Self {
        Self {
            resolver: AliasResolver::new(upstreams, hierarchy),
            routes,
            endpoints: EndpointSelector::new(policy.allow_http_upstream),
            ssrf,
            auth,
            policy,
            transport,
            limiters: Arc::new(DashMap::new()),
        }
    }

    /// The deployment policy the data plane enforces.
    #[must_use]
    pub const fn policy(&self) -> &ProxyPolicy {
        &self.policy
    }

    /// Carry `request` to its upstream.
    ///
    /// # Errors
    /// Every failure is a [`DomainError`] of `DESIGN` §3.3: the alias, the
    /// route, the quota, the CORS posture, the endpoint selection, a plugin
    /// refusal, or the upstream exchange itself.
    pub async fn forward(&self, request: ForwardRequest) -> Result<ForwardOutcome, DomainError> {
        let origin = request.context.header("origin").map(ToOwned::to_owned);
        let resolved = self
            .resolver
            .resolve(request.context.tenant, &request.context.alias)
            .await?;
        let routes = self
            .routes
            .list_by_upstream(resolved.selected.tenant_id, resolved.selected.id)
            .await?;
        let selected = select_route(
            &resolved.selected,
            &routes,
            &request.method,
            &request.context.path,
            &query_pairs(&request.context.query),
        )?;

        let decision = self.enforce_rate_limit(&resolved, &selected.route, &request.context)?;
        Self::enforce_cors(&resolved.selected, &selected.route, &request)?;

        let endpoint = self.select_endpoint(&resolved.selected, request.target_host.as_deref())?;

        let mut context = request.context;
        // The passthrough rules govern the caller's headers (`DESIGN` §"Headers
        // Transformation"), so what the plugins write from here on is kept
        // apart: a credential a plugin produced must reach the upstream even
        // when the caller's header of the same name would be dropped.
        let inbound_headers = context.headers.clone();
        let auth = self.auth.resolve(
            resolved.selected.auth.as_ref(),
            (context.tenant, context.subject),
        )?;
        let outcome = match auth {
            Some(plugin) => plugin.authenticate(&mut context).await?,
            None => AuthOutcome::default(),
        };
        Self::apply_forwarded(&mut context, &outcome.forwarded_headers);

        let chain = Chain::resolve(&resolved.selected, &selected.route)?;
        chain.guard_request(&context).await?;
        chain.transform_request(&mut context).await?;
        let produced = produced_headers(&inbound_headers, &context.headers);

        let headers = outbound_request_headers(
            &context.headers,
            &endpoint.authority(),
            resolved
                .selected
                .headers
                .as_ref()
                .and_then(|headers| headers.request.as_ref()),
            request.upgrade,
            &produced,
        );
        let trace_id = context.trace_id.clone();
        let exchange = Self::upstream_request(
            &endpoint,
            &selected,
            &context.query,
            &request.method,
            &headers,
            if request.upgrade {
                axum::body::Body::empty()
            } else {
                request.body
            },
        )?;

        if request.upgrade {
            let (parts, stream) = self.transport.tunnel(exchange).await?;
            return Ok(ForwardOutcome::Tunnel { parts, stream });
        }

        let upstream = self.transport.send(exchange).await?;
        let (parts, body) = upstream.into_parts();
        let mut response = ResponseContext {
            status: parts.status.as_u16(),
            headers: flattened(&parts.headers),
            trace_id,
        };
        let observed = response.headers.clone();
        chain.guard_response(&response).await?;
        chain.transform_response(&mut response).await?;

        let answer = Answered {
            observed,
            transformed: response,
            origin,
            body,
            decision,
        };
        Ok(ForwardOutcome::Response(Self::client_response(
            &resolved.selected,
            &selected.route,
            &parts,
            answer,
        )))
    }

    /// Spend the caller's quota (`ADR`-0003).
    ///
    /// The effective configuration is the most derived override tightened by
    /// every ancestor that enforces its limit (`DESIGN` §"Hierarchical
    /// Configuration": `effective_rate = min(selected_rate, route_rate,
    /// all_ancestor_enforced_rates)`), and the verdict is reported so the
    /// caller can read the quota in `X-RateLimit-*`.
    fn enforce_rate_limit(
        &self,
        resolved: &ResolvedUpstream,
        route: &Route,
        context: &ProxyContext,
    ) -> Result<Option<RateDecision>, DomainError> {
        let Some(config) = effective_rate_limit(resolved, route) else {
            return Ok(None);
        };
        let route_scoped = route.rate_limit.is_some();
        let id = if route_scoped {
            route.id
        } else {
            resolved.selected.id
        };
        let mut entry = self
            .limiters
            .entry(id)
            .or_insert_with(|| (config.clone(), RateLimiter::new(&config)));
        // A replaced upstream or route re-arms its quota: the cached limiter is
        // only reused while the configuration that created it is still current.
        if entry.0 != config {
            *entry = (config.clone(), RateLimiter::new(&config));
        }
        let limiter: &RateLimiter = &entry.1;
        let counter = limiter.key(
            context.tenant,
            context.subject,
            context.header("x-forwarded-for"),
            Some(route.id),
        );
        limiter.check(&counter).map(Some)
    }

    /// Validate the origin and the method of an actual cross-origin request
    /// (`ADR`-0004). A preflight never reaches the upstream.
    fn enforce_cors(
        upstream: &Upstream,
        route: &Route,
        request: &ForwardRequest,
    ) -> Result<(), DomainError> {
        let Some(config) = route.cors.as_ref().or(upstream.cors.as_ref()) else {
            return Ok(());
        };
        CorsPolicy::new(config).check(
            request.context.header("origin"),
            &request.method.as_str().to_ascii_uppercase(),
        )
    }

    /// Pick the endpoint and enforce the outbound posture (`DESIGN` §4.4).
    fn select_endpoint(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<SelectedEndpoint, DomainError> {
        let endpoint = self.endpoints.select(upstream, target_host)?;
        self.ssrf.check(&endpoint.host, endpoint.requires_tls())?;
        Ok(endpoint)
    }

    /// Fold the credentials an auth plugin produced into the request.
    fn apply_forwarded(context: &mut ProxyContext, forwarded: &BTreeMap<String, String>) {
        for (name, value) in forwarded {
            let key = name.trim().to_ascii_lowercase();
            if let Some(query_name) = key.strip_prefix("query:") {
                context.query.retain(|(existing, _)| existing != query_name);
                context.query.push((query_name.to_owned(), value.clone()));
            } else {
                context.headers.insert(key, value.clone());
            }
        }
    }

    /// The outbound request the transport dials.
    fn upstream_request(
        endpoint: &SelectedEndpoint,
        selected: &SelectedRoute,
        query: &[(String, String)],
        method: &http::Method,
        headers: &HeaderMap,
        body: axum::body::Body,
    ) -> Result<http::Request<axum::body::Body>, DomainError> {
        let mut builder = http::Request::builder()
            .method(method.clone())
            .version(http::Version::HTTP_11)
            .uri(outbound_uri(
                &endpoint.url(),
                &selected.outbound_path,
                query,
            ));
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        builder
            .body(body)
            .map_err(|error| DomainError::ProtocolError {
                detail: format!("the outbound request is not a valid HTTP message: {error}"),
                trace_id: None,
            })
    }

    /// The answer the caller receives: upstream status, headers with the
    /// response rules applied, and the body still streaming.
    fn client_response(
        upstream: &Upstream,
        route: &Route,
        parts: &http::response::Parts,
        answer: Answered,
    ) -> http::Response<axum::body::Body> {
        let rules = upstream
            .headers
            .as_ref()
            .and_then(|headers| headers.response.as_ref());
        let mut headers = outbound_response_headers(&parts.headers, rules);
        merge_transformed(&mut headers, &answer.observed, &answer.transformed);
        // The size the upstream declared travels with the answer: without it the
        // streamed body would be re-framed as chunked (`DESIGN` §3.2).
        if let Some(size) = parts
            .extensions
            .get::<crate::infra::proxy::transport::ExactBodySize>()
            && parts.status.is_success()
        {
            headers.remove(http::header::CONTENT_LENGTH);
            headers.insert(
                http::header::CONTENT_LENGTH,
                http::HeaderValue::from(size.0),
            );
        }
        if let Some(decision) = answer.decision {
            // `ADR`-0003, More Information: the quota is reported even when the
            // request is admitted, so a caller can pace itself.
            for (name, value) in rate_limit_headers(&decision) {
                if let Some(name) = name {
                    headers.insert(name, value);
                }
            }
        }

        let cors = route
            .cors
            .as_ref()
            .or(upstream.cors.as_ref())
            .map(CorsPolicy::new);
        let cors_headers =
            cors.and_then(|policy| policy.response_headers(answer.origin.as_deref()));

        let mut builder = http::Response::builder().status(parts.status);
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        if let Some(cors) = cors_headers {
            for (name, value) in cors.into_headers() {
                builder = builder.header(name, value);
            }
        }
        builder.body(answer.body).unwrap_or_else(|error| {
            tracing::error!(diagnostic = %error, "proxied response is not a valid HTTP message");
            http::Response::builder()
                .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::empty())
                .unwrap_or_else(|_| http::Response::new(axum::body::Body::empty()))
        })
    }
}

/// The `X-RateLimit-*` headers of a verdict (`ADR`-0003, More Information).
fn rate_limit_headers(
    decision: &RateDecision,
) -> Vec<(Option<http::HeaderName>, http::HeaderValue)> {
    [
        ("x-ratelimit-limit", decision.limit),
        ("x-ratelimit-remaining", decision.remaining),
        ("x-ratelimit-reset", decision.reset),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        let name = http::HeaderName::from_bytes(name.as_bytes()).ok()?;
        let value = http::HeaderValue::from(value);
        Some((Some(name), value))
    })
    .collect()
}

/// The effective rate-limit configuration of a proxied request.
///
/// `DESIGN` §"Hierarchical Configuration" merges the enforced limits of the
/// chain into `effective_rate = min(selected_rate, route_rate,
/// all_ancestor_enforced_rates)`: the most derived configuration is the base
/// and an ancestor whose `sharing` is `enforce` may only tighten it.
fn effective_rate_limit(
    resolved: &ResolvedUpstream,
    route: &Route,
) -> Option<crate::domain::model::RateLimitConfig> {
    let mut effective = route
        .rate_limit
        .clone()
        .or_else(|| resolved.selected.rate_limit.clone())?;
    for enforced in std::iter::once(&resolved.selected)
        .chain(resolved.ancestors.iter())
        .filter_map(|upstream| upstream.rate_limit.as_ref())
        .filter(|config| config.sharing == crate::domain::model::SharingMode::Enforce)
    {
        if throughput(enforced) < throughput(&effective) {
            effective.sustained = enforced.sustained.clone();
        }
        effective.burst = match (effective.burst, enforced.burst) {
            (Some(mine), Some(theirs)) => Some(crate::domain::model::Burst {
                capacity: mine.capacity.min(theirs.capacity),
            }),
            (mine, theirs) => mine.or(theirs),
        };
    }
    Some(effective)
}

/// The sustained throughput of a configuration, in tokens per second.
#[allow(clippy::cast_precision_loss)]
fn throughput(config: &crate::domain::model::RateLimitConfig) -> f64 {
    let window = match config.sustained.window {
        crate::domain::model::RateWindow::Second => 1.0,
        crate::domain::model::RateWindow::Minute => 60.0,
        crate::domain::model::RateWindow::Hour => 3_600.0,
        crate::domain::model::RateWindow::Day => 86_400.0,
    };
    config.sustained.rate as f64 / window
}

/// The headers the gateway produced, as the names that bypass the passthrough
/// rules: a header a plugin wrote, replaced or removed from the caller's set.
fn produced_headers(
    inbound: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
) -> std::collections::BTreeSet<String> {
    now.iter()
        .filter(|(name, value)| inbound.get(*name) != Some(*value))
        .map(|(name, _)| name.clone())
        .collect()
}

/// What the upstream answered, before the caller's response is built.
struct Answered {
    /// The upstream headers as they arrived, before a transform touched them.
    observed: BTreeMap<String, String>,
    /// The response view the guard and transform plugins produced.
    transformed: ResponseContext,
    /// The `Origin` of a cross-origin call, for the CORS response headers.
    origin: Option<String>,
    /// The upstream body, still streaming.
    body: axum::body::Body,
    /// The quota verdict of the request, when the upstream is rate limited.
    decision: Option<RateDecision>,
}

/// Apply to the wire headers what a transform changed on the response view.
///
/// Upstream headers keep their multiplicity; a header a transform added,
/// replaced or removed is the only one rewritten.
fn merge_transformed(
    headers: &mut HeaderMap,
    observed: &BTreeMap<String, String>,
    transformed: &ResponseContext,
) {
    for (name, value) in &transformed.headers {
        if observed.get(name) != Some(value)
            && let Ok(name) = http::HeaderName::from_bytes(name.as_bytes())
            && let Ok(value) = http::HeaderValue::from_str(value)
        {
            headers.insert(name, value);
        }
    }
    for name in observed.keys() {
        if !transformed.headers.contains_key(name)
            && let Ok(name) = http::HeaderName::from_bytes(name.as_bytes())
        {
            headers.remove(&name);
        }
    }
}

/// The plugin chain of one request: upstream first, then route (`ADR`-0002).
struct Chain {
    guards: Vec<Arc<dyn crate::domain::plugin::GuardPlugin>>,
    transforms: Vec<Arc<dyn crate::domain::plugin::TransformPlugin>>,
}

impl Chain {
    /// Resolve the guard and transform chain of the upstream and the route.
    ///
    /// # Errors
    /// Returns [`DomainError::PluginNotFound`] for a reference no registry
    /// serves.
    fn resolve(upstream: &Upstream, route: &Route) -> Result<Self, DomainError> {
        let mut guards = Vec::new();
        let mut transforms = Vec::new();
        for entry in upstream
            .plugins
            .iter()
            .flat_map(|chain| &chain.items)
            .chain(route.plugins.iter().flat_map(|chain| &chain.items))
        {
            let reference = entry.plugin_ref();
            if reference.starts_with(TRANSFORM_PLUGIN_TYPE) {
                transforms.push(TransformPluginRegistry::resolve(reference, entry.config())?);
            } else if reference.starts_with(GUARD_PLUGIN_TYPE) {
                guards.push(GuardPluginRegistry::resolve(reference, entry.config())?);
            } else {
                // A bare UUID (or an untyped id): the guard registry answers
                // with the canonical `PluginNotFound`.
                guards.push(GuardPluginRegistry::resolve(reference, entry.config())?);
            }
        }
        Ok(Self { guards, transforms })
    }

    /// Run the guards of the request, in chain order.
    ///
    /// # Errors
    /// Returns [`DomainError::AccessDenied`] for the first refusal, plus
    /// whatever a guard reports itself.
    async fn guard_request(&self, context: &ProxyContext) -> Result<(), DomainError> {
        for guard in &self.guards {
            if let GuardDecision::Deny(detail) = guard.guard_request(context).await? {
                return Err(DomainError::AccessDenied { detail });
            }
        }
        Ok(())
    }

    /// Run the guards of the response, in chain order.
    ///
    /// # Errors
    /// Returns [`DomainError::DownstreamError`] for the first refusal.
    async fn guard_response(&self, context: &ResponseContext) -> Result<(), DomainError> {
        for guard in &self.guards {
            if let GuardDecision::Deny(detail) = guard.guard_response(context).await? {
                return Err(DomainError::DownstreamError { detail });
            }
        }
        Ok(())
    }

    /// Run the request transforms, in chain order.
    ///
    /// # Errors
    /// Propagates the transform's own failure.
    async fn transform_request(&self, context: &mut ProxyContext) -> Result<(), DomainError> {
        for transform in &self.transforms {
            transform.transform_request(context).await?;
        }
        Ok(())
    }

    /// Run the response transforms, in chain order.
    ///
    /// # Errors
    /// Propagates the transform's own failure.
    async fn transform_response(&self, context: &mut ResponseContext) -> Result<(), DomainError> {
        for transform in &self.transforms {
            transform.transform_response(context).await?;
        }
        Ok(())
    }
}

/// The headers of `parts`, as the `BTreeMap` the plugins read.
fn flattened(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
        })
        .collect()
}

/// The query of a [`ProxyContext`], as the borrowed pairs the route matcher
/// validates.
fn query_pairs(query: &[(String, String)]) -> Vec<(&str, &str)> {
    query
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}
