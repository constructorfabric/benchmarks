//! The axum handler of the proxy data plane
//! ([DESIGN.md](../../../docs/DESIGN.md) "Proxy Request Flow").
//!
//! [`proxy_request`] is one handler for every method and both proxy paths; it
//! drives the pure decisions of [`crate::domain::proxy`] in the order DESIGN.md
//! fixes and hands the dispatch to [`crate::infra::http_client`]:
//!
//! ```text
//! caller → alias → upstream → route → CORS → endpoint → SSRF → scheme
//!        → rate limit → header plan → plugins (request) → dispatch
//!        → plugins (response) → CORS headers
//! ```
//!
//! A CORS preflight never enters that pipeline: it is answered at the handler
//! level, permissively, before anything is resolved
//! ([ADR-0004](../../../docs/ADR/0004-cors.md) "Preflight Request Handling").
//!
//! The response body is never buffered: whatever the upstream streamed is
//! handed back as an [`axum::body::Body`], so a `text/event-stream` arrives
//! chunk by chunk. A request that carries an upgrade handshake
//! (`Connection: upgrade` + `Upgrade: websocket`/`wt`) takes the tunnel of
//! [`crate::infra::upgrade`] instead: the handshake is forwarded untouched, the
//! upstream's `101` is relayed and the two sockets are spliced for the lifetime
//! of the session.
//!
//! Every response carries `X-OAGW-Error-Source`, and every request ends in an
//! audit log entry and a `oagw_requests_total` increment.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use opentelemetry::KeyValue;
use tenant_resolver_sdk::{BarrierMode, GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit::ClientHub;
use toolkit_canonical_errors::Problem;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::merger;
use crate::domain::model::Alias;
use crate::domain::model::{EndpointScheme, Route, Upstream, UpstreamEndpoint};
use crate::domain::proxy::{
    self, CORS_METHOD_NOT_ALLOWED_GTS_ID, CORS_ORIGIN_NOT_ALLOWED_GTS_ID, ERROR_SOURCE_GATEWAY,
    ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, ProxyError, RequestHeaderPlan, ResponseHeaderPlan,
    TARGET_HOST_HEADER,
};
use crate::domain::service::Service;
use crate::gear::OagwState;
use crate::infra::cors::{self, CorsGrant};
use crate::infra::http_client::ProxyCall;
use crate::infra::http_client::ProxyClient;
use crate::infra::plugins::{ErrorView, PluginExecution, RequestContext};
use crate::infra::ratelimit::{LimitResource, RateLimitIdentity};
use crate::infra::upgrade;

/// The proxy path of an alias without a suffix path.
pub const PROXY_ALIAS_PATH: &str = "/oagw/v1/proxy/{alias}";
/// The proxy path of an alias with a suffix path.
pub const PROXY_SUFFIX_PATH: &str = "/oagw/v1/proxy/{alias}/{*path}";
/// The prefix every proxied request path carries.
const PROXY_PATH_PREFIX: &str = "/oagw/v1/proxy/";
/// The route pattern reported for a request no route matched.
const UNMATCHED_ROUTE: &str = "/oagw/v1/proxy/{alias}";
/// The GTS instance id of the `cf.oagw.ssrf.blocked.v1` row.
const SSRF_BLOCKED_GTS_ID: &str = "cf.core.errors.err.v1~cf.oagw.ssrf.blocked.v1";

/// Shared state of the proxy data plane, layered onto the router as an
/// extension.
pub struct ProxyState {
    /// The gear state, swapped as a whole on a configuration reload.
    pub gear: Arc<ArcSwap<OagwState>>,
    /// The management service: the store and the route-enablement overlay.
    pub service: Arc<Service>,
    /// The client hub the tenant hierarchy resolves through.
    pub client_hub: Arc<ClientHub>,
    /// The shared outbound client, with the round-robin counter.
    pub client: ProxyClient,
}

impl std::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyState")
            .field("config", &self.gear.load().config)
            .finish_non_exhaustive()
    }
}

/// The upstream a request resolved to, and the decisions taken about it.
struct Resolved {
    /// The upstream the request goes to.
    upstream: Upstream,
    /// The route that matched; a request with no matching route is a 404
    /// before the endpoint is even selected.
    route: Route,
    /// The path the upstream receives.
    upstream_path: String,
}

/// A proxied request that reached the response phase.
struct Served {
    response: Response,
    host: String,
    route_path: String,
    upstream_id: Uuid,
    route_id: Option<Uuid>,
    /// Machine-readable identity of a gateway error the plugins produced.
    error_code: Option<String>,
}

/// What one proxied request ended as, with the ids the audit log reports.
struct Outcome {
    response: Response,
    host: String,
    route_path: String,
    upstream_id: Option<Uuid>,
    route_id: Option<Uuid>,
    error_code: Option<String>,
}

/// The proxy endpoint: one handler for every method of both proxy paths.
///
/// The platform middleware injects the caller's [`SecurityContext`] as an
/// extension; a request without one is a 401.
pub async fn proxy_request(
    caller: Option<axum::Extension<SecurityContext>>,
    axum::Extension(state): axum::Extension<Arc<ProxyState>>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let request_id = request_id(&request);
    let tenant_id = caller
        .as_ref()
        .map(|axum::Extension(ctx)| ctx.subject_tenant_id());
    let origin = cors::origin_of(request.headers());

    // ADR-0004 "Preflight Request Handling": a browser sends no credentials on
    // a preflight, so there is no tenant context to resolve an upstream with.
    // The answer is permissive and needs nothing resolved; the origin and the
    // method are enforced on the actual request that follows it.
    let outcome = if cors::is_preflight(&method, request.headers()) {
        preflight(request.headers())
    } else {
        match caller {
            Some(axum::Extension(security)) => finalize(
                serve(&state, security, request).await,
                &path,
                origin.as_deref(),
            ),
            None => finalize(Err(unauthenticated()), &path, origin.as_deref()),
        }
    };
    audit(&outcome, &method, &path, tenant_id, &request_id, started);
    count_request(&state, &outcome, &method);
    outcome.response
}

/// The permissive preflight answer: a body-less `204` echoing what the browser
/// asked for, produced by the gateway with no upstream behind it.
fn preflight(request_headers: &HeaderMap) -> Outcome {
    let mut headers = cors::preflight_headers(request_headers);
    headers.insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    *response.headers_mut() = headers;
    Outcome {
        response,
        host: String::new(),
        route_path: UNMATCHED_ROUTE.to_owned(),
        upstream_id: None,
        route_id: None,
        error_code: None,
    }
}

/// Turns a `serve` result into an [`Outcome`], rendering a gateway error as
/// `application/problem+json` with the ADR-0007 source header.
///
/// Every response to a cross-origin request is marked `Vary: Origin`: a cache
/// must not hand a CORS answer to a request that asked for none, whatever the
/// request ended as.
fn finalize(outcome: Result<Served, ProxyError>, path: &str, origin: Option<&str>) -> Outcome {
    match outcome {
        Ok(served) => {
            let mut response = served.response;
            if origin.is_some() {
                cors::add_vary_origin(response.headers_mut());
            }
            Outcome {
                response,
                host: served.host,
                route_path: served.route_path,
                upstream_id: Some(served.upstream_id),
                route_id: served.route_id,
                error_code: served.error_code,
            }
        }
        Err(error) => {
            let mut response = gateway_error(&error, path);
            if origin.is_some() {
                cors::add_vary_origin(response.headers_mut());
            }
            Outcome {
                response,
                host: host_of(&error),
                route_path: UNMATCHED_ROUTE.to_owned(),
                upstream_id: None,
                route_id: None,
                error_code: Some(error_code(&error)),
            }
        }
    }
}

/// The host label of a failed request: the alias it named, when it got far
/// enough to resolve one.
fn host_of(error: &ProxyError) -> String {
    match error {
        ProxyError::UpstreamDisabled { alias } => alias.clone(),
        ProxyError::Domain(OagwError::RouteNotFound { alias }) => alias.clone(),
        _ => String::new(),
    }
}

/// The 401 the proxy returns for a request without a security context.
fn unauthenticated() -> ProxyError {
    ProxyError::Domain(OagwError::AuthenticationFailed {
        message: "no security context on the request: the caller is not authenticated".to_owned(),
    })
}

/// The machine-readable identity of an error, for the audit log: the DESIGN.md
/// row id of the failure.
fn error_code(error: &ProxyError) -> String {
    match error {
        ProxyError::UpstreamDisabled { .. } => proxy::UPSTREAM_DISABLED_GTS_ID.to_owned(),
        ProxyError::RouteDisabled { .. } => proxy::ROUTE_DISABLED_GTS_ID.to_owned(),
        ProxyError::DownstreamUnavailable { .. } => proxy::DOWNSTREAM_UNAVAILABLE_GTS_ID.to_owned(),
        ProxyError::SsrfBlocked { .. } => SSRF_BLOCKED_GTS_ID.to_owned(),
        ProxyError::CorsOriginNotAllowed { .. } => CORS_ORIGIN_NOT_ALLOWED_GTS_ID.to_owned(),
        ProxyError::CorsMethodNotAllowed { .. } => CORS_METHOD_NOT_ALLOWED_GTS_ID.to_owned(),
        ProxyError::MissingTargetHost { .. } => {
            crate::domain::error::MISSING_TARGET_HOST_GTS_ID.to_owned()
        }
        ProxyError::InvalidTargetHost { .. } => {
            crate::domain::error::INVALID_TARGET_HOST_GTS_ID.to_owned()
        }
        ProxyError::UnknownTargetHost { .. } => {
            crate::domain::error::UNKNOWN_TARGET_HOST_GTS_ID.to_owned()
        }
        ProxyError::RateLimited { .. } => {
            crate::domain::error::RATE_LIMIT_EXCEEDED_GTS_ID.to_owned()
        }
        ProxyError::Domain(error) => error.gts_id().to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Runs the whole proxy pipeline for one authenticated request.
///
/// The request is kept whole until dispatch: an upgrade request has to hand its
/// own socket to [`crate::infra::upgrade::tunnel`], which nobody can do once the
/// handler's response future is gone.
async fn serve(
    state: &ProxyState,
    security: SecurityContext,
    mut request: Request,
) -> Result<Served, ProxyError> {
    let config = state.gear.load().config.clone();
    let method = request.method().as_str().to_owned();
    let query = request
        .uri()
        .query()
        .map(str::to_owned)
        .filter(|query| !query.is_empty());
    let (alias, request_path) = split_proxy_path(request.uri().path())?;
    let target_host = target_host_header(request.headers());
    let inbound = inbound_headers(request.headers());
    let origin = cors::origin_of(request.headers());
    let is_upgrade = upgrade::is_upgrade_request(request.headers());
    // The handshake headers are restored over the plan below, which strips them
    // as hop-by-hop for every other request class.
    let handshake = is_upgrade.then(|| upgrade::handshake_headers(request.headers()));
    let declared = declared_length(request.headers())?;

    let resolved = resolve(state, &security, &alias, &method, &request_path).await?;
    // ADR-0004 "Actual Request Handling": the origin and the method of a
    // cross-origin request are validated against the merged CORS policy after
    // the upstream is resolved and before anything is forwarded to it.
    let grant = cors::for_upstream_route(&resolved.upstream, Some(&resolved.route))
        .map(|policy| CorsGrant::new(policy, origin.clone()));
    if let Some(grant) = grant.as_ref() {
        grant.check(&method)?;
    }
    let selected = proxy::select_endpoint(
        &resolved.upstream,
        target_host.as_deref(),
        state.client.next_round_robin(),
    )?;
    check_scheme(&config, selected.endpoint)?;
    check_ssrf(&config, selected.endpoint)?;

    let policy = merger::for_upstream_route(&resolved.upstream, Some(&resolved.route));
    let limit_headers = rate_limit(state, &security, &resolved, policy.as_ref())?;
    let chain = plugin_chain(state, &security, &resolved)?;
    let host = resolved
        .upstream
        .alias
        .as_ref()
        .map_or_else(String::new, |alias| alias.to_string());

    let mut context = RequestContext::new(
        security,
        resolved.upstream.id.unwrap_or_default(),
        &resolved.upstream_path,
    );
    context.route_id = resolved.route.id;
    context.query.clone_from(&query);
    context.request_headers = apply_request_plan(&proxy::request_header_plan(
        selected.endpoint,
        &inbound,
        resolved.upstream.headers.as_ref(),
        None,
    ));
    if let Some(handshake) = handshake.as_ref() {
        context.request_headers =
            upgrade::planned_request_headers(&context.request_headers, handshake);
    }
    if let Some(mut error) = request_rejection(&chain, &mut context).await {
        if let Err(report) = chain.run_error(&mut context, &mut error).await {
            return Err(ProxyError::Domain(report));
        }
        if let Some(grant) = grant.as_ref() {
            grant.apply(&mut error.headers);
        }
        return Ok(rejection(&resolved, &host, &error));
    }

    let call = ProxyCall {
        method,
        scheme: selected.endpoint.scheme.as_str().to_owned(),
        authority: proxy::endpoint_authority(selected.endpoint),
        path: resolved.upstream_path.clone(),
        query,
        declared_length: declared,
        limit: proxy::MAX_PROXY_BODY_BYTES,
        timeout: Duration::from_secs(config.proxy_timeout_secs),
    };
    if is_upgrade {
        let mut served = tunnel_request(
            &state.client,
            &call,
            &mut request,
            &context,
            limit_headers,
            &resolved,
            &host,
        )
        .await?;
        if let Some(grant) = grant.as_ref() {
            grant.apply(served.response.headers_mut());
        }
        return Ok(served);
    }
    let body = request.into_body();
    let response = state
        .client
        .send(&call, context.request_headers.clone(), body)
        .await?;
    assemble(
        &chain,
        &mut context,
        response,
        &resolved,
        &host,
        limit_headers,
        grant.as_ref(),
    )
    .await
}

/// The upgrade branch of the pipeline: forwards the handshake, relays the
/// upstream's `101` and splices the two sockets for the session's lifetime.
///
/// The response side of the plugin chain and the response header rules do not
/// run here: a `101` *is* the handshake, and rewriting it — or buffering the
/// session to let a transform look at it — would break the protocol that was
/// just switched to. [`ERROR_SOURCE_HEADER`] still names the side that
/// answered, as it does on every proxied response.
///
/// What is forwarded is the handshake, and nothing else: the raw query string
/// and the method travel with it, and the bytes a caller sent after the
/// handshake are not part of it.
///
/// The caller's half of the socket is claimed before the response is returned:
/// that is the last moment at which hyper still offers it.
async fn tunnel_request(
    client: &ProxyClient,
    call: &ProxyCall,
    request: &mut Request,
    context: &RequestContext,
    limit_headers: HeaderMap,
    resolved: &Resolved,
    host: &str,
) -> Result<Served, ProxyError> {
    let mut upstream = client
        .send_upgrade(call, context.request_headers.clone())
        .await?;
    let client_io = hyper::upgrade::on(request);
    let upstream_io = upstream.on_upgrade();
    let status = upstream.status();

    let mut headers = upgrade::relayed_response_headers(upstream.headers());
    for (name, value) in &limit_headers {
        headers.insert(name, value.clone());
    }
    headers.insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
    );

    let mut response = Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .map_err(|error| {
            ProxyError::from(OagwError::Validation {
                message: format!("the upgrade response could not be built: {error}"),
            })
        })?;
    *response.headers_mut() = headers;
    tokio::spawn(splice(
        client_io,
        upstream_io,
        host.to_owned(),
        call.path.clone(),
    ));
    Ok(Served {
        response,
        host: host.to_owned(),
        route_path: route_path_of(resolved),
        upstream_id: resolved.upstream.id.unwrap_or_default(),
        route_id: resolved.route.id,
        error_code: None,
    })
}

/// Pumps one tunnel to its end, for the log rather than for a caller: the
/// response has already been sent, so a broken session is only traceable.
async fn splice(
    client_io: hyper::upgrade::OnUpgrade,
    upstream_io: hyper::upgrade::OnUpgrade,
    host: String,
    path: String,
) {
    match upgrade::tunnel(client_io, upstream_io).await {
        Ok((to_upstream, to_client)) => {
            tracing::debug!(host, path, to_upstream, to_client, "the tunnel closed");
        }
        Err(error) => {
            tracing::debug!(host, path, error = %error, "the tunnel broke");
        }
    }
}

/// A `Served` for an upstream response, after the response side of the plugin
/// chain and the response header plan ran on it.
async fn assemble(
    chain: &PluginExecution,
    context: &mut RequestContext,
    response: crate::infra::http_client::ProxyResponse,
    resolved: &Resolved,
    host: &str,
    limit_headers: HeaderMap,
    grant: Option<&CorsGrant>,
) -> Result<Served, ProxyError> {
    let (status, mut headers, body) = response.into_parts();

    // The chain sees the upstream's headers before a transform or the response
    // rules mutate them. The view carries no body: the built-in response
    // transforms are header-only, and buffering the stream to offer it to them
    // would defeat the end-to-end streaming.
    let mut view = crate::infra::plugins::UpstreamResponseView::new(status);
    view.headers.clone_from(&headers);
    let decision = chain.run_response(context, &mut view).await?;
    if let Some(mut error) = rejection_of(decision) {
        if let Err(report) = chain.run_error(context, &mut error).await {
            return Err(ProxyError::Domain(report));
        }
        if let Some(grant) = grant {
            grant.apply(&mut error.headers);
        }
        return Ok(rejection(resolved, host, &error));
    }

    strip_hop_by_hop(&mut headers);
    if resolved.upstream.headers.is_some() {
        let plan = proxy::response_header_plan(resolved.upstream.headers.as_ref(), None);
        apply_response_plan(&mut headers, &plan);
    }
    for (name, value) in limit_headers.iter() {
        headers.insert(name, value.clone());
    }
    // ADR-0007: the header names the side that produced the response, and is
    // present on every one of them — `upstream` for whatever came back, however
    // it statuses, `gateway` for what the pipeline generated instead.
    headers.insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
    );
    // The caller's origin, when CORS governs the pair, learns which of its
    // origins may read this response and which headers it may see.
    if let Some(grant) = grant {
        grant.apply(&mut headers);
    }

    let mut response = Response::builder()
        .status(status)
        .body(axum::body::Body::new(body))
        .map_err(|error| {
            ProxyError::from(OagwError::Validation {
                message: format!("the upstream response could not be built: {error}"),
            })
        })?;
    *response.headers_mut() = headers;
    Ok(Served {
        response,
        host: host.to_owned(),
        route_path: route_path_of(resolved),
        upstream_id: resolved.upstream.id.unwrap_or_default(),
        route_id: resolved.route.id,
        error_code: None,
    })
}

/// The `http.route` label of a request: the matched route's path, or the proxy
/// pattern when the matched rule carries no HTTP match.
fn route_path_of(resolved: &Resolved) -> String {
    resolved
        .route
        .match_rule
        .http
        .as_ref()
        .map_or_else(|| UNMATCHED_ROUTE.to_owned(), |http| http.path.clone())
}

/// The guard rejection of a decision, as the error view the transforms run on.
fn rejection_of(decision: crate::infra::plugins::GuardDecision) -> Option<ErrorView> {
    match decision {
        crate::infra::plugins::GuardDecision::Allow => None,
        crate::infra::plugins::GuardDecision::Reject {
            status,
            error_code,
            message,
        } => Some(ErrorView {
            status,
            error_code,
            message,
            headers: HeaderMap::new(),
        }),
    }
}

/// Runs the request side of the chain, returning the guard's rejection if one
/// rejected.
async fn request_rejection(
    chain: &PluginExecution,
    context: &mut RequestContext,
) -> Option<ErrorView> {
    match chain.run_request(context).await {
        Ok(decision) => rejection_of(decision),
        Err(error) => Some(ErrorView {
            status: StatusCode::from_u16(error.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            error_code: error.gts_id().to_owned(),
            message: error.to_string(),
            headers: HeaderMap::new(),
        }),
    }
}

/// A `Served` for a guard rejection, which is a gateway error: the guard chose
/// the status and the headers, and the error transforms already ran.
fn rejection(resolved: &Resolved, host: &str, error: &ErrorView) -> Served {
    Served {
        response: error_response(error),
        host: host.to_owned(),
        route_path: route_path_of(resolved),
        upstream_id: resolved.upstream.id.unwrap_or_default(),
        route_id: resolved.route.id,
        error_code: Some(error.error_code.clone()),
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolves the alias to an upstream, then the request to a route.
async fn resolve(
    state: &ProxyState,
    security: &SecurityContext,
    alias: &Alias,
    method: &str,
    request_path: &str,
) -> Result<Resolved, ProxyError> {
    let chain = tenant_chain(&state.client_hub, security).await?;
    let store = state.service.store();
    let not_found = || OagwError::RouteNotFound {
        alias: alias.to_string(),
    };
    let found = chain.iter().find_map(|tenant| {
        store
            .find_upstream_by_alias(*tenant, alias)
            .map(|upstream| (*tenant, upstream))
    });
    let Some((owner_tenant, upstream)) = found else {
        return Err(not_found().into());
    };
    let Some(upstream_id) = upstream.id else {
        return Err(not_found().into());
    };
    if !upstream.enabled {
        return Err(ProxyError::UpstreamDisabled {
            alias: alias.to_string(),
        });
    }

    let routes = store.find_routes_by_upstream(owner_tenant, upstream_id);
    let Some((route, upstream_path)) = proxy::match_route(&routes, method, request_path) else {
        // The route table is the access policy of the upstream: a path the
        // routes do not name is not proxied, whatever the upstream would answer.
        return Err(not_found().into());
    };
    if let Some(route_id) = route.id
        && !state.service.is_route_enabled(owner_tenant, route_id)
    {
        return Err(ProxyError::RouteDisabled { route_id });
    }
    Ok(Resolved {
        upstream,
        route: route.clone(),
        upstream_path,
    })
}

/// The caller's tenant chain: the caller's tenant first, then its ancestors
/// (mirrors [`Service::ancestor_chain`], which the management use cases own).
async fn tenant_chain(
    client_hub: &Arc<ClientHub>,
    security: &SecurityContext,
) -> Result<Vec<Uuid>, ProxyError> {
    let tenant = security.subject_tenant_id();
    let Some(client) = client_hub.try_get::<dyn TenantResolverClient>() else {
        return Ok(vec![tenant]);
    };
    let options = GetAncestorsOptions {
        barrier_mode: BarrierMode::Ignore,
    };
    let response = client
        .get_ancestors(security, TenantId(tenant), &options)
        .await
        .map_err(|error| OagwError::LinkUnavailable {
            message: format!("the tenant hierarchy could not be read: {error}"),
        })?;
    let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
    chain.push(tenant);
    chain.extend(response.ancestors.iter().map(|ancestor| ancestor.id.0));
    Ok(chain)
}

/// The plugin chain of a request: the upstream's auth plugin, then the bound
/// guards and transforms, upstream-bound before route-bound.
fn plugin_chain(
    state: &ProxyState,
    security: &SecurityContext,
    resolved: &Resolved,
) -> Result<PluginExecution, ProxyError> {
    let gear = state.gear.load();
    let bindings = |bindings: Option<&crate::domain::model::PluginBindings>| -> Vec<crate::domain::model::PluginBinding> {
        bindings.map_or_else(Vec::new, |chain| chain.items.clone())
    };
    PluginExecution::resolve(
        &gear.plugins,
        Some(&gear.plugin_catalog),
        security.subject_tenant_id(),
        resolved.upstream.auth.as_ref(),
        &bindings(resolved.upstream.plugins.as_ref()),
        &bindings(resolved.route.plugins.as_ref()),
    )
    .map_err(ProxyError::from)
}

// ---------------------------------------------------------------------------
// Policy checks
// ---------------------------------------------------------------------------

/// Rejects an endpoint scheme this build cannot dial.
///
/// `http` is legal whenever `allow_http_upstream` allows the plaintext dial;
/// the TLS schemes need a TLS connector this build does not install, so they
/// are reported as a gateway error rather than mis-served.
fn check_scheme(config: &OagwConfig, endpoint: &UpstreamEndpoint) -> Result<(), ProxyError> {
    match endpoint.scheme {
        EndpointScheme::Http if config.allow_http_upstream => Ok(()),
        EndpointScheme::Http => Err(OagwError::Validation {
            message: "plaintext 'http' endpoints are not allowed: allow_http_upstream is false"
                .to_owned(),
        }
        .into()),
        _ => Err(OagwError::Validation {
            message: format!(
                "the '{}' scheme of '{}' is not supported by this build: only 'http' endpoints \
                 are proxied",
                endpoint.scheme.as_str(),
                endpoint.host
            ),
        }
        .into()),
    }
}

/// Rejects a target the SSRF policy forbids, when the policy is enabled.
fn check_ssrf(config: &OagwConfig, endpoint: &UpstreamEndpoint) -> Result<(), ProxyError> {
    if !config.ssrf_policy.enabled {
        return Ok(());
    }
    match proxy::ssrf_rejection(&endpoint.host) {
        Some(reason) => Err(ProxyError::SsrfBlocked {
            message: format!("'{}' is a {reason}", endpoint.host),
        }),
        None => Ok(()),
    }
}

/// The `X-RateLimit-*` / `Retry-After` headers of the request's rate-limit
/// decision, or an error when the limit is exceeded.
fn rate_limit(
    state: &ProxyState,
    security: &SecurityContext,
    resolved: &Resolved,
    policy: Option<&merger::EffectiveRateLimit>,
) -> Result<HeaderMap, ProxyError> {
    let Some(policy) = policy else {
        return Ok(HeaderMap::new());
    };
    let Some(upstream_id) = resolved.upstream.id else {
        return Ok(HeaderMap::new());
    };
    let resource = resolved.route.id.map_or_else(
        || LimitResource::upstream(upstream_id),
        LimitResource::route,
    );
    let mut identity = RateLimitIdentity::new(security.subject_tenant_id(), upstream_id)
        .with_subject(security.subject_id());
    if let Some(route_id) = resolved.route.id {
        identity = identity.with_route(route_id);
    }
    let decision = state
        .gear
        .load()
        .rate_limiter
        .check(policy, &resource, &identity);
    if !decision.allowed {
        return Err(ProxyError::RateLimited {
            error: decision.error(),
            headers: header_pairs(&decision.headers(policy)),
        });
    }
    Ok(decision.headers(policy))
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// The inbound headers of a request, as `(name, value)` pairs.
fn inbound_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    header_pairs(headers)
}

/// Any header map, as `(name, value)` pairs.
fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// The `X-OAGW-Target-Host` value of a request, read before it is stripped.
fn target_host_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Validates the framing headers of a request and returns the declared body
/// length the transport frames the outbound body with: `None` for an empty or a
/// chunked body, the `Content-Length` for a sized one.
fn declared_length(headers: &HeaderMap) -> Result<Option<u64>, ProxyError> {
    let content_length = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok());
    let transfer_encoding = headers
        .get(axum::http::header::TRANSFER_ENCODING)
        .and_then(|value| value.to_str().ok());
    Ok(
        match proxy::validate_body_shape(content_length, transfer_encoding)? {
            proxy::BodyShape::Empty | proxy::BodyShape::Chunked => None,
            proxy::BodyShape::Sized(length) => Some(length),
        },
    )
}

/// The `alias` and the request path after it, from the proxy path.
///
/// `/oagw/v1/proxy/api.vendor.com/v1/resource` resolves to the alias
/// `api.vendor.com` and the path `/v1/resource`.
fn split_proxy_path(path: &str) -> Result<(Alias, String), ProxyError> {
    let rest = path
        .strip_prefix(PROXY_PATH_PREFIX)
        .ok_or_else(|| OagwError::Validation {
            message: format!("'{path}' is not a proxied request path"),
        })?;
    let (segment, suffix) = rest.split_once('/').unwrap_or((rest, ""));
    let alias = Alias::try_new(proxy::normalize_alias(segment)).map_err(|_| not_found(segment))?;
    let request_path = format!("/{suffix}");
    Ok((alias, request_path))
}

/// The 404 of a path segment that is not an alias.
fn not_found(segment: &str) -> OagwError {
    OagwError::RouteNotFound {
        alias: segment.to_owned(),
    }
}

/// Applies the outbound header plan to an empty header map, in order: the
/// forwarded inbound headers, then the endpoint `Host`, then `set`, then `add`.
fn apply_request_plan(plan: &RequestHeaderPlan) -> HeaderMap {
    let mut headers = HeaderMap::new();
    // The endpoint replaces the caller's `Host`, which the plan already dropped
    // from `forward`: host and port, as the upstream route expects to see.
    put_header(&mut headers, "host", &plan.host, true);
    for (name, value) in plan.forward.iter().chain(plan.set.iter()) {
        put_header(&mut headers, name, value, is_set(&plan.set, name));
    }
    for (name, value) in &plan.add {
        put_header(&mut headers, name, value, false);
    }
    headers
}

/// `true` when `(name, _)` is in `set`, whose entries the plan replaces.
fn is_set(set: &[(String, String)], name: &str) -> bool {
    set.iter().any(|(candidate, _)| candidate == name)
}

/// Writes one planned header, skipping a name or value that is not a valid HTTP
/// token (the plugin chain runs after the plan and can still add valid ones).
fn put_header(headers: &mut HeaderMap, name: &str, value: &str, replace: bool) {
    let (Ok(parsed_name), Ok(parsed_value)) = (
        HeaderName::from_lowercase(name.as_bytes()),
        HeaderValue::from_str(value),
    ) else {
        tracing::debug!(header = name, "skipping a header that is not a valid token");
        return;
    };
    if replace {
        headers.remove(&parsed_name);
    }
    headers.append(parsed_name, parsed_value);
}

/// Applies the response header plan, in order: `remove`, then `set`, then `add`.
fn apply_response_plan(headers: &mut HeaderMap, plan: &ResponseHeaderPlan) {
    for name in &plan.remove {
        if let Ok(parsed) = HeaderName::from_lowercase(name.as_bytes()) {
            headers.remove(&parsed);
        }
    }
    for (name, value) in plan.set.iter().chain(plan.add.iter()) {
        put_header(headers, name, value, is_set(&plan.set, name));
    }
}

/// Drops the hop-by-hop headers the upstream sent, which never reach the caller.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| proxy::is_hop_by_hop(name.as_str()))
        .cloned()
        .collect();
    for name in names {
        headers.remove(&name);
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Renders a gateway error as `application/problem+json` with the ADR-0007
/// source header and the request path as the problem `instance`.
fn gateway_error(error: &ProxyError, path: &str) -> Response {
    let mut problem = Problem::from(error.canonical());
    problem.instance = Some(path.to_owned());
    let mut response_headers = HeaderMap::new();
    match error {
        ProxyError::MissingTargetHost { valid_hosts, .. } => {
            put_context(&mut problem, "valid_hosts", valid_hosts.clone());
        }
        ProxyError::InvalidTargetHost { invalid_value, .. } => {
            put_context(&mut problem, "invalid_value", invalid_value.clone());
        }
        ProxyError::UnknownTargetHost {
            invalid_value,
            valid_hosts,
            ..
        } => {
            put_context(&mut problem, "invalid_value", invalid_value.clone());
            put_context(&mut problem, "valid_hosts", valid_hosts.clone());
        }
        // ADR-0003: the 429 carries the decision's headers, `Retry-After`
        // always and the `X-RateLimit-*` set when the policy enables them.
        ProxyError::RateLimited { headers, .. } => {
            for (name, value) in headers {
                put_header(&mut response_headers, name, value, false);
            }
        }
        _ => {}
    }
    let mut response = problem.into_response();
    for (name, value) in response_headers {
        if let Some(name) = name {
            response.headers_mut().insert(name, value);
        }
    }
    response.headers_mut().insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// Renders a guard rejection, after its transforms, as the proxy's own error.
fn error_response(view: &ErrorView) -> Response {
    let mut response = Response::builder()
        .status(view.status)
        .body(axum::body::Body::from(view.message.clone()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    *response.headers_mut() = view.headers.clone();
    response.headers_mut().insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// Adds an extension field to the problem's `context` object.
fn put_context(problem: &mut Problem, key: &str, value: impl Into<serde_json::Value>) {
    if !problem.context.is_object() {
        problem.context = serde_json::Value::Object(serde_json::Map::new());
    }
    problem.context[key] = value.into();
}

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

/// The request id of a proxied call: the inbound one when it carried a
/// non-blank `X-Request-Id`, a fresh one otherwise.
fn request_id(request: &Request) -> String {
    request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| Uuid::now_v7().to_string(), str::to_owned)
}

/// The audit log entry of one proxied request (DESIGN.md "Audit Logging").
///
/// No PII, no secrets: only ids, the path, the status and the timing.
fn audit(
    outcome: &Outcome,
    method: &str,
    path: &str,
    tenant_id: Option<Uuid>,
    request_id: &str,
    started: Instant,
) {
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        target: "oagw::audit",
        event = "proxy_request",
        request_id,
        tenant_id = tenant_id.map(|tenant| tenant.to_string()),
        principal_id = tracing::field::Empty,
        host = %outcome.host,
        path,
        method,
        status = outcome.response.status().as_u16(),
        duration_ms,
        upstream_id = outcome.upstream_id.map(|id| id.to_string()),
        route_id = outcome.route_id.map(|id| id.to_string()),
        error_type = outcome.error_code,
        "proxied request"
    );
}

/// The `oagw_requests_total` counter, built once on the global meter.
fn requests_total() -> &'static opentelemetry::metrics::Counter<u64> {
    static COUNTER: std::sync::OnceLock<opentelemetry::metrics::Counter<u64>> =
        std::sync::OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("oagw")
            .u64_counter("oagw_requests_total")
            .with_description("Proxied requests by host, method, route and status code")
            .with_unit("{request}")
            .build()
    })
}

/// Increments `oagw_requests_total` for one proxied request.
fn count_request(state: &ProxyState, outcome: &Outcome, method: &str) {
    let status = outcome.response.status().as_u16().to_string();
    requests_total().add(
        1,
        &[
            KeyValue::new("host", outcome.host.clone()),
            KeyValue::new("http.request.method", normalized_method(state, method)),
            KeyValue::new("http.route", outcome.route_path.clone()),
            KeyValue::new("http.response.status_code", status),
        ],
    );
}

/// The method label of the metric: a standard verb, or `_OTHER`.
fn normalized_method(state: &ProxyState, method: &str) -> String {
    let _ = state;
    match method {
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" => method.to_owned(),
        _ => "_OTHER".to_owned(),
    }
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
