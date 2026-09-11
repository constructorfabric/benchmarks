//! The data plane: resolving, validating, rate-limiting, running the plugin chain and
//! relaying a request to its upstream.
//!
//! Relaying is byte-streaming, not buffered: a `text/event-stream` body is handed to the
//! caller frame by frame as the upstream produces it, and a `Connection: Upgrade`
//! exchange is spliced into a WebSocket bridge so neither side sees a buffered round trip.

pub mod cors;
pub mod headers;
pub mod observability;
pub mod sse;
pub mod target;
pub mod ws;

use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Uri};
use http_body_util::BodyExt;

use crate::config::OagwConfig;
use crate::domain::route::Route;
use crate::domain::upstream::{RateLimit, Upstream};
use crate::error::{ErrorKind, OagwError};
use crate::plugins::token_cache::TokenCache;
use crate::plugins::{Chain, InjectedCredential, PluginContext};
use crate::ratelimit::{Rate, RateLimiter};
use crate::security::{CredentialResolver, SecurityContextHolder};
use crate::store::OagwStore;
use tenant_resolver_sdk::{TenantResolverClient, TenantResolverError};
/// Hard ceiling on a buffered request body.
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// The outbound HTTP client: HTTP/1.1 over TLS or cleartext.
pub type HttpClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    axum::body::Body,
>;

/// Builds the shared outbound client.
///
/// Both cleartext and TLS are enabled at the connector level; whether a plaintext dial is
/// allowed is a policy decision taken per request, not a connector capability.
///
/// # Errors
///
/// Returns the connector builder's error when the system certificate store cannot be
/// read.
pub fn build_client() -> anyhow::Result<HttpClient> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .build();
    Ok(hyper_util::client::legacy::Client::builder(
        hyper_util::rt::TokioExecutor::new(),
    )
    .pool_idle_timeout(std::time::Duration::from_secs(90))
    .build(connector))
}

/// The relay, holding the control-plane state and the outbound client.
pub struct ProxyService {
    store: Arc<OagwStore>,
    credentials: Arc<dyn CredentialResolver>,
    token_cache: Arc<TokenCache>,
    limiter: Arc<RateLimiter>,
    config: OagwConfig,
    client: HttpClient,
    tenants: Option<Arc<dyn TenantResolverClient>>,
    metrics: Arc<observability::Metrics>,
    /// The round-robin cursor each upstream's endpoint pool is walked with.
    rotation: target::Rotation,
}

impl std::fmt::Debug for ProxyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyService")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProxyService {
    /// Builds a service over the given control-plane state.
    #[must_use]
    pub fn new(
        store: Arc<OagwStore>,
        credentials: Arc<dyn CredentialResolver>,
        token_cache: Arc<TokenCache>,
        limiter: Arc<RateLimiter>,
        config: OagwConfig,
        tenants: Option<Arc<dyn TenantResolverClient>>,
        client: HttpClient,
    ) -> Self {
        Self {
            store,
            credentials,
            token_cache,
            limiter,
            config,
            client,
            tenants,
            metrics: Arc::new(observability::Metrics::new()),
            rotation: target::Rotation::default(),
        }
    }

    /// Resolves the caller's tenant chain, closest tenant first.
    ///
    /// Without a tenant resolver the chain is the caller's own tenant alone, which is the
    /// correct behaviour for a single-tenant deployment.
    ///
    /// # Errors
    ///
    /// Returns the resolver's error when one is wired and it fails, other than the
    /// resolver's authoritative answer that the caller's tenant is not in the hierarchy
    /// at all, which is not a failure — see [`resolve_chain`].
    pub async fn chain_for(
        &self,
        ctx: &toolkit_security::SecurityContext,
    ) -> anyhow::Result<crate::store::TenantChain> {
        resolve_chain(self.tenants.as_deref(), ctx).await
    }

    /// The control-plane store.
    #[must_use]
    pub const fn store(&self) -> &Arc<OagwStore> {
        &self.store
    }

    /// The gear configuration.
    #[must_use]
    pub const fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The outbound client.
    #[must_use]
    pub const fn client(&self) -> &HttpClient {
        &self.client
    }

    /// The metric registry the relay records into.
    #[must_use]
    pub const fn metrics(&self) -> &Arc<observability::Metrics> {
        &self.metrics
    }

    /// The round-robin cursor the relay walks each upstream's endpoint pool with.
    ///
    /// Exposed for the tests of the load-balancing rule, which read the cursor directly
    /// rather than dialling a pool to watch it move.
    #[must_use]
    pub const fn rotation(&self) -> &target::Rotation {
        &self.rotation
    }
}

/// Resolves the caller's tenant chain, closest tenant first.
///
/// Two kinds of answer are distinct, and confusing them is either a fault or a bypass:
///
/// - the resolver's `TenantNotFound` is an *authoritative* answer about hierarchy, not a
///   failure of the resolver: a tenant it has never heard of has no ancestors, so the
///   caller's chain is itself alone. The request proceeds and the caller sees nothing of
///   anyone else's — which is the tenancy isolation the design asks for (TR-09), not an
///   internal error.
/// - every other resolver error is an *outage*. Degrading to a single-tenant chain then
///   would silently drop every enforced ancestor constraint (FR-020) for as long as the
///   resolver was down, so the request fails instead.
async fn resolve_chain(
    resolver: Option<&dyn TenantResolverClient>,
    ctx: &toolkit_security::SecurityContext,
) -> anyhow::Result<crate::store::TenantChain> {
    let own = ctx.subject_tenant_id();
    let Some(resolver) = resolver else {
        return Ok(crate::store::TenantChain::single(own));
    };
    let response = match resolver
        .get_ancestors(
            ctx,
            tenant_resolver_sdk::TenantId(own),
            &tenant_resolver_sdk::GetAncestorsOptions::default(),
        )
        .await
    {
        Ok(response) => response,
        Err(TenantResolverError::TenantNotFound { .. }) => {
            return Ok(crate::store::TenantChain::single(own));
        }
        Err(err) => return Err(err.into()),
    };
    let mut chain = vec![own];
    for ancestor in response.ancestors {
        chain.push(ancestor.id.0);
    }
    Ok(crate::store::TenantChain::new(chain))
}

/// Everything the relay needs to know about one inbound request.
pub struct ProxyRequest {
    pub method: Method,
    pub alias: String,
    /// The path after the alias segment.
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub client_ip: String,
    pub security: SecurityContextHolder,
    /// Set when the caller asked for a WebSocket upgrade.
    pub upgrade: Option<axum::extract::ws::WebSocketUpgrade>,
}

/// The result of a relay: the upstream response, or a gateway error.
pub type ProxyResult = Result<Response, OagwError>;

/// Relays one request end to end.
///
/// Every outcome — including a refusal taken before a route was matched — goes through
/// [`finish`], so a request the gateway refused is still logged and its problem document
/// still names the trace it belongs to.
///
/// # Errors
///
/// Returns the canonical error the design assigns to each failure mode; a
/// gateway-generated error is never mistaken for an upstream one.
pub async fn relay(service: &ProxyService, request: &mut ProxyRequest) -> ProxyResult {
    let started = std::time::Instant::now();
    let (route, outbound, result) = match relay_steps(service, request).await {
        Ok((route, outbound, response)) => (Some(route), outbound, Ok(response)),
        Err(err) => (None, HeaderMap::new(), Err(err)),
    };
    let unmatched = Route::default();
    finish(
        service,
        request,
        &outbound,
        route.as_ref().unwrap_or(&unmatched),
        result,
        started,
    )
}

/// The body of [`relay`]: resolves, validates, dials and relays.
///
/// Returns the matched route and the header set the gateway settled on for the upstream
/// call alongside the response, so the caller can record both; an error returns without
/// them, because a refusal may have happened before either existed.
///
/// # Errors
///
/// Returns the canonical error the design assigns to each failure mode; a
/// gateway-generated error is never mistaken for an upstream one.
async fn relay_steps(
    service: &ProxyService,
    request: &mut ProxyRequest,
) -> Result<(Route, HeaderMap, Response), OagwError> {
    let chain = request.security.chain();

    // 1. Alias resolution, closest tenant first.
    let upstream = service
        .store
        .find_upstream_by_alias(&request.alias, &chain)
        .ok_or_else(|| {
            OagwError::new(
                ErrorKind::RouteNotFound,
                format!("no upstream answers the alias `{}`", request.alias),
            )
            .with_path(&request.path)
        })?;
    if !upstream.enabled {
        return Err(OagwError::new(
            ErrorKind::LinkUnavailable,
            format!("upstream `{}` is disabled", upstream.alias),
        )
        .with_upstream_id(&upstream.id));
    }

    // A preflight never reaches here: the proxy handler answers it before resolving
    // the caller, because a browser preflight carries no credentials to resolve with.
    // A preflight that does arrive is answered permissively anyway rather than with a
    // 405, which is what a route table without an OPTIONS route would produce.
    if cors::is_preflight(&request.method, &request.headers) {
        let response = cors::preflight_response(&request.headers);
        let unmatched = Route::default();
        return Ok((unmatched, HeaderMap::new(), response));
    }

    // 2. Origin and method allowlists, before anything else is decided about the
    //    request (ADR-0004): a cross-origin caller is rejected with a `403` whether or
    //    not a route would have matched it.
    validate_cors(&upstream, request)?;

    // 3. Route matching, longest path first.
    let route = match_route(&service.store, &upstream, &chain, request)?;

    // 4. Request validation.
    validate(&upstream, &route, request)?;

    // 5. Rate limiting.
    if let Some(policy) = effective_rate_limit(&upstream, &route) {
        let key = RateLimiter::key(&policy, &request.security, &route.id, &request.client_ip);
        let decision = service.limiter.check(&key, &policy);
        if !decision.allowed() {
            service.metrics.record_rate_limited(&upstream.alias, &route.http_match().map_or_else(|| "*".to_owned(), |m| m.path.clone()));
            return Err(OagwError::new(
                ErrorKind::RateLimitExceeded,
                "rate limit exceeded for this upstream",
            )
            .with_retry_after(decision.retry_after_secs)
            .with_rate_limit(crate::error::RateLimitQuota {
                limit: Rate::from_policy(&policy).capacity,
                remaining: decision.remaining,
                reset_in_secs: decision.retry_after_secs,
            })
            .with_upstream_id(&upstream.id)
            .with_path(&request.path));
        }
    }

    // 6. Endpoint resolution. The caller may pin the endpoint with `X-OAGW-Target-Host`;
    //    otherwise the pool is walked round-robin (ADR-0001) so that successive requests
    //    are distributed across its members.
    let pinned = request
        .headers
        .get(target::TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok());
    let rotation = service.rotation.next(&upstream.id, upstream.server.endpoints.len());
    let destination = target::resolve(&upstream, pinned, rotation)?;
    target::check_ssrf(&service.config.ssrf_policy, &destination.endpoint.host)
        .map_err(|err| err.with_upstream_id(&upstream.id).with_path(&request.path))?;
    if !service.config.permits_plaintext(&destination.endpoint.scheme) {
        return Err(OagwError::new(
            ErrorKind::LinkUnavailable,
            format!(
                "plaintext upstream scheme `{}` is not permitted by the gear policy",
                destination.endpoint.scheme
            ),
        )
        .with_upstream_id(&upstream.id));
    }

    // 7. The plugin chain: auth, then guards, then transforms.
    let bindings = Chain::new(&upstream.plugins, &route.plugins);
    let config = serde_json::Map::new();
    let plugin_ctx = PluginContext {
        security: &request.security,
        config: &config,
        upstream_id: &upstream.id,
        host: &destination.authority,
        path: &request.path,
        // The request's own phases do not read it: the `request_id` transform is what
        // settles it, and it runs below.
        request_id: "",
        credentials: service.credentials.as_ref(),
        token_cache: &service.token_cache,
    };
    let injected = bindings.run_auth(&plugin_ctx).await?;
    let mut outbound = headers::build_request_headers(
        &request.headers,
        &upstream.headers,
        &destination.authority,
        &credential_headers(&injected),
    );
    bindings.run_request_guards(&plugin_ctx, &outbound)?;
    bindings.run_request_transforms(&plugin_ctx, &mut outbound)?;

    // The request's own phases have settled the correlation identifier now, so the
    // response phases carry the same one back to the caller (FR-018).
    let settled_id = correlation_id(&request.headers, &outbound);
    let response_ctx = PluginContext { request_id: &settled_id, ..plugin_ctx };

    // 8. Dial.
    let forwarded_path = forward_path(&route, &request.path);
    if is_websocket_upgrade(&request.headers) {
        let upgrade = request.upgrade.take();
        let response = match upgrade {
            Some(upgrade) => {
                ws::bridge(upgrade, &destination, &forwarded_path, &request.query).await
            }
            None => Err(OagwError::new(
                ErrorKind::ProtocolError,
                "the connection cannot be upgraded on this server",
            )),
        };
        return response.map(|response| (route, outbound, response));
    }

    let url = build_url(&destination, &forwarded_path, &request.query);
    let response = dispatch(
        service.client(),
        request.method.clone(),
        url,
        outbound.clone(),
        request.body.clone(),
    )
    .await?;

    // 9. Relay, streaming.
    let response = relay_response(
        &upstream,
        &request.method,
        &request.headers,
        &bindings,
        &response_ctx,
        response,
    )?;
    Ok((route, outbound, response))
}

/// Closes one relay: emits the structured log record and the counters for it, then hands
/// the outcome back to the caller.
///
/// The record is built from the resolved fields only, so no body, query string, header or
/// credential value can reach the log.
fn finish(
    service: &ProxyService,
    request: &ProxyRequest,
    outbound: &HeaderMap,
    route: &Route,
    result: ProxyResult,
    started: std::time::Instant,
) -> ProxyResult {
    let duration = started.elapsed();
    let route_pattern = route
        .http_match()
        .map_or_else(|| "*".to_owned(), |m| m.path.clone());
    let status = match &result {
        Ok(response) => response.status().as_u16(),
        Err(err) => err.kind().status(),
    };
    let record = observability::RequestRecord {
        request_id: correlation_id(&request.headers, outbound),
        tenant_id: request.security.own_tenant().to_string(),
        host: request.alias.clone(),
        route: route_pattern,
        method: request.method.as_str().to_owned(),
        status,
        duration,
        error_type: result
            .as_ref()
            .err()
            .map(|err| err.kind().gts_id().to_owned()),
    };
    let mut result = result;
    if let Err(err) = &mut result {
        // FR-029: the problem document names the trace, so an operator can follow the
        // same identifier the log record and the relayed response carry.
        *err = err.clone().with_trace_id(&record.request_id);
    }
    if let Err(err) = &result {
        service
            .metrics
            .record_error(&request.alias, &record.route, err.kind().gts_id());
    }
    service.metrics.record(&record);
    observability::log_request(&record);
    result
}

/// The correlation identifier a proxied request was actually given.
///
/// The identifier the gateway settles on for the upstream call is the one an operator can
/// trace: the `request_id` transform generates one when the caller sent none, and that
/// generated value lives in the outbound header set alone. Reading the caller's inbound
/// value only would leave every such request logged with an empty identifier, which is
/// not the record US8/AC1 describes. An identifier is a routing-adjacent string, not a
/// credential, so it is safe for the log.
#[must_use]
pub fn correlation_id(inbound: &HeaderMap, outbound: &HeaderMap) -> String {
    for headers in [outbound, inbound] {
        if let Some(value) = headers
            .get(crate::error::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            return value.to_owned();
        }
    }
    String::new()
}

/// The header/value pairs the auth plugins want set upstream.
#[must_use]
pub fn credential_headers(injected: &[InjectedCredential]) -> Vec<(String, String)> {
    injected
        .iter()
        .map(|credential| {
            (
                credential.header.clone(),
                String::from_utf8_lossy(&credential.value).into_owned(),
            )
        })
        .collect()
}

/// Builds the outbound URL from the resolved target and the forwarded path.
#[must_use]
pub fn build_url(destination: &target::Target, path: &str, query: &str) -> String {
    if query.is_empty() {
        format!("{}{}", destination.origin(), path)
    } else {
        format!("{}{}?{}", destination.origin(), path, query)
    }
}

/// Sends a buffered request and returns the upstream response.
///
/// The body is streamed out of the response as it arrives; only the request body is
/// buffered, and only because its size has to be checked before a connection is made.
async fn dispatch(
    client: &HttpClient,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Bytes,
) -> Result<http::Response<hyper::body::Incoming>, OagwError> {
    let uri: Uri = url.parse().map_err(|err| {
        OagwError::new(
            ErrorKind::ValidationError,
            format!("outbound URL is not valid: {err}"),
        )
    })?;
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(Body::from(body))
        .map_err(|err| {
            OagwError::new(ErrorKind::ProtocolError, format!("request is not buildable: {err}"))
        })?;

    let response = client.request(request).await.map_err(|err| {
        OagwError::new(
            ErrorKind::DownstreamError,
            format!("upstream request failed: {err}"),
        )
    })?;
    Ok(response)
}

/// Builds the caller-facing response from the upstream response.
fn relay_response(
    upstream: &Upstream,
    method: &Method,
    inbound: &HeaderMap,
    chain: &Chain,
    ctx: &PluginContext<'_>,
    response: http::Response<hyper::body::Incoming>,
) -> ProxyResult {
    let (parts, body) = response.into_parts();
    let mut builder = Response::builder().status(parts.status);
    // The response guards assert about the upstream's own answer, before the gateway's
    // static response rules rewrite it: a header the gateway was told to set itself is
    // not evidence the upstream sent one.
    chain.run_response_guards(ctx, &parts.headers)?;
    let mut upstream_headers = headers::build_response_headers(
        &parts.headers,
        &upstream.headers.response,
        &[],
    );
    // The response transforms then rewrite what the caller receives, which is where the
    // correlation identifier is handed back (FR-018).
    chain.run_response_transforms(ctx, &mut upstream_headers)?;
    for (name, value) in &upstream_headers {
        builder = builder.header(name, value);
    }
    let mut out = builder
        .body(Body::from_stream(body.into_data_stream()))
        .map_err(|err| {
            OagwError::new(ErrorKind::ProtocolError, format!("invalid response: {err}"))
        })?;
    headers::set(
        out.headers_mut(),
        crate::error::ERROR_SOURCE_HEADER,
        crate::error::ERROR_SOURCE_UPSTREAM,
    );
    if let Some(origin) = origin(inbound) {
        if let Some(cors) = &upstream.cors {
            cors::apply_to_response(cors, &origin, method, &mut out);
        }
    }
    Ok(out)
}

/// Picks the route whose path is the longest prefix of the requested path.
///
/// A route that names no methods is not a candidate; neither is one whose methods exclude
/// the request's. Ties are broken by the lower priority value, which the store's listing
/// order already guarantees.
///
/// A path the routes know but no route accepts the method for is *not* the same answer as
/// a path no route names: the first is the caller using the wrong verb on a resource that
/// exists, which the design reports as a validation error (US2/AC9), the second is a
/// route that does not exist, reported as not-found (US2/AC3).
///
/// # Errors
///
/// Returns [`ErrorKind::RouteNotFound`] when no enabled route matches the path suffix,
/// and a validation error when a route matches the path but none accepts the method.
pub fn match_route(
    store: &OagwStore,
    upstream: &Upstream,
    chain: &crate::store::TenantChain,
    request: &ProxyRequest,
) -> Result<Route, OagwError> {
    let candidates = store.routes_for_upstream(&upstream.id, chain);

    let mut best: Option<&Route> = None;
    let mut path_matched = false;
    for route in &candidates {
        let Some(http) = route.http_match() else {
            continue;
        };
        if !path_matches(&http.path, &request.path) {
            continue;
        }
        path_matched = true;
        if !http.methods.iter().any(|m| m.eq_ignore_ascii_case(request.method.as_str())) {
            continue;
        }
        // Longest path prefix wins; ties go to the lower priority value.
        let better = match best {
            None => true,
            Some(current) => {
                let current_len = current.http_match().map_or(0, |m| m.path.len());
                if http.path.len() > current_len {
                    true
                } else {
                    http.path.len() == current_len && route.priority < current.priority
                }
            }
        };
        if better {
            best = Some(route);
        }
    }

    if let Some(route) = best {
        return Ok(route.clone());
    }
    // The upstream advertises the path but takes no method this request may use. The
    // message names only what the caller sent, never the route table it collided with.
    if path_matched {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("no route on this upstream accepts {} for {}", request.method, request.path),
        )
        .with_upstream_id(&upstream.id)
        .with_path(&request.path));
    }
    Err(OagwError::new(
        ErrorKind::RouteNotFound,
        format!(
            "no route on upstream `{}` matches {} {}",
            upstream.alias, request.method, request.path
        ),
    )
    .with_upstream_id(&upstream.id)
    .with_path(&request.path))
}

/// Whether a route's configured path prefix matches the requested path.
///
/// An exact match always wins; otherwise the route path must be a proper segment prefix of
/// the request path, so `/v1` matches `/v1/chat` but not `/v1chat`.
#[must_use]
pub fn path_matches(configured: &str, requested: &str) -> bool {
    let configured = configured.trim_end_matches('/');
    if configured.is_empty() {
        return requested.starts_with('/');
    }
    if requested == configured || requested == format!("{configured}/") {
        return true;
    }
    requested.starts_with(&format!("{configured}/"))
}

/// Validates the request against the upstream's and route's rules.
///
/// # Errors
///
/// Returns a validation error for a disallowed method, an unknown query parameter, a
/// rejected path suffix, a cross-origin request outside the allowlist, or a body that
/// contradicts its own framing.
pub fn validate(upstream: &Upstream, route: &Route, request: &ProxyRequest) -> Result<(), OagwError> {
    let Some(http) = route.http_match() else {
        return Ok(());
    };

    if !http
        .methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(request.method.as_str()))
    {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("method {} is not allowed on this route", request.method),
        ));
    }

    if http.path_suffix_mode == crate::domain::route::PathSuffixMode::Disabled
        && !request.path.is_empty()
        && request.path != http.path
    {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            "path suffixes are not accepted on this route",
        ));
    }

    if !http.query_allowlist.is_empty() {
        for (name, _) in form_urlencoded::parse(request.query.as_bytes()) {
            let name = name.to_string();
            if !http.query_allowlist.contains(&name) {
                return Err(OagwError::new(
                    ErrorKind::ValidationError,
                    format!("query parameter `{name}` is not allowed"),
                ));
            }
        }
    }

    validate_body(request)?;

    if let Some(cors) = &upstream.cors {
        if let Some(origin) = origin(&request.headers) {
            // A cross-origin caller is checked against the allowlists before the
            // request reaches the upstream (ADR-0004).
            if !cors::origin_allowed(cors, &origin) {
                return Err(OagwError::new(
                    ErrorKind::CorsOriginNotAllowed,
                    format!("origin `{origin}` is not in this upstream's allowed origins"),
                )
                .with_upstream_id(&upstream.id)
                .with_path(&request.path));
            }
        }
    }
    Ok(())
}

/// Enforces a cross-origin caller's origin and method against the upstream allowlists.
///
/// Runs before route matching (ADR-0004), so a `403` answers a disallowed caller
/// regardless of what the route table would have said about its path or method.
fn validate_cors(upstream: &Upstream, request: &ProxyRequest) -> Result<(), OagwError> {
    let Some(cors) = &upstream.cors else {
        return Ok(());
    };
    let Some(origin) = origin(&request.headers) else {
        return Ok(());
    };
    if !cors::origin_allowed(cors, &origin) {
        return Err(OagwError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("origin `{origin}` is not in this upstream's allowed origins"),
        )
        .with_upstream_id(&upstream.id)
        .with_path(&request.path));
    }
    if !cors::method_allowed(cors, &request.method) {
        return Err(OagwError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("method {} is not in this upstream's allowed methods", request.method),
        )
        .with_upstream_id(&upstream.id)
        .with_path(&request.path));
    }
    Ok(())
}

/// Checks the body's framing against its declared length and the hard size ceiling.
///
/// # Errors
///
/// Returns [`ErrorKind::PayloadTooLarge`] past the ceiling and a validation error for a
/// malformed or contradictory `Content-Length`.
pub fn validate_body(request: &ProxyRequest) -> Result<(), OagwError> {
    if request.body.len() > MAX_BODY_BYTES {
        return Err(OagwError::new(
            ErrorKind::PayloadTooLarge,
            format!("request body exceeds the {MAX_BODY_BYTES} byte limit"),
        ));
    }
    if let Some(declared) = request
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
    {
        let declared: usize = declared.trim().parse().map_err(|_| {
            OagwError::new(
                ErrorKind::ValidationError,
                "Content-Length must be a valid integer",
            )
        })?;
        if declared != request.body.len() {
            return Err(OagwError::new(
                ErrorKind::ValidationError,
                format!(
                    "Content-Length declares {declared} bytes but the body carries {}",
                    request.body.len()
                ),
            ));
        }
    }
    validate_transfer_encoding(&request.headers)?;
    Ok(())
}

/// Rejects a transfer encoding the relay cannot carry.
///
/// `chunked` is the one encoding the gateway forwards, and the only one the inbound
/// server will have decoded into the body it handed over; anything else (an
/// `HTTP/2`-style extension, a compressed transfer coding) arrives with a body whose
/// framing the gateway cannot reason about, so it is refused before the plugin chain runs
/// rather than silently stripped and forwarded (DESIGN, request-validation table).
///
/// # Errors
///
/// Returns [`ErrorKind::ValidationError`] for any encoding but `chunked`.
fn validate_transfer_encoding(headers: &HeaderMap) -> Result<(), OagwError> {
    for value in headers.get_all(http::header::TRANSFER_ENCODING) {
        let Some(declared) = value.to_str().ok() else {
            return Err(unsupported_transfer_encoding("an undecodable value"));
        };
        for coding in declared.split(',') {
            let coding = coding.trim();
            // A coding may carry parameters (`chunked; foo=bar`); the name ends at the
            // first semicolon.
            let name = coding.split(';').next().map_or(coding, str::trim);
            if !name.eq_ignore_ascii_case("chunked") {
                return Err(unsupported_transfer_encoding(name));
            }
        }
    }
    Ok(())
}

fn unsupported_transfer_encoding(name: &str) -> OagwError {
    OagwError::new(
        ErrorKind::ValidationError,
        format!("transfer encoding `{name}` is not supported, only `chunked` is"),
    )
}

/// The stricter of the upstream's and the route's rate-limit policies.
#[must_use]
pub fn effective_rate_limit(upstream: &Upstream, route: &Route) -> Option<RateLimit> {
    match (&upstream.rate_limit, &route.rate_limit) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(a), Some(b)) => {
            let a_per_sec = per_second(&a.sustained);
            let b_per_sec = per_second(&b.sustained);
            Some(if a_per_sec <= b_per_sec { a.clone() } else { b.clone() })
        }
    }
}

/// A sustained rate expressed per second.
#[must_use]
pub fn per_second(
    rate: &crate::domain::upstream::SustainedRate,
) -> u64 {
    let window = rate.window.duration().as_secs().max(1);
    rate.rate.div_ceil(window)
}

/// The path handed to the upstream, honouring the route's suffix mode.
#[must_use]
pub fn forward_path(route: &Route, requested: &str) -> String {
    let Some(http) = route.http_match() else {
        return requested.to_owned();
    };
    match http.path_suffix_mode {
        crate::domain::route::PathSuffixMode::Append => {
            if requested.len() > http.path.len() && requested.starts_with(&http.path) {
                requested.to_owned()
            } else {
                http.path.clone()
            }
        }
        crate::domain::route::PathSuffixMode::Disabled => http.path.clone(),
    }
}

/// Whether the inbound request carries a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        && headers
            .get(http::header::UPGRADE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// The caller's `Origin` header, when present.
#[must_use]
pub fn origin(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
#[path = "headers_tests.rs"]
mod headers_tests;

#[cfg(test)]
#[path = "cors_tests.rs"]
mod cors_tests;

#[cfg(test)]
#[path = "sse_tests.rs"]
mod sse_tests;

#[cfg(test)]
#[path = "ws_tests.rs"]
mod ws_tests;

#[cfg(test)]
#[path = "observability_tests.rs"]
mod observability_tests;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;
