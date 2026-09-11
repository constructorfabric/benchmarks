//! Handlers of the data plane (`/oagw/v1/proxy/...`).
//!
//! The catch-all is registered by hand rather than through `OperationBuilder`: the proxied path
//! space is defined by the stored routes, not by the gear's own OpenAPI document.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::ApiState;
use crate::domain::error::DomainError;
use crate::domain::plugin::{ProxyRequest, ProxyResponse};
use crate::infra::api::problem;
use crate::infra::api::problem::stamp_gateway_source;
use crate::infra::proxy::service::{
    DataPlane, Resolved, UpstreamBody, apply_response_rules, map_plugin_error, valid_hosts,
};

/// Path prefix every proxied call sits under.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// Mount the proxy routes.
#[must_use = "the routes must be merged into the gear's router"]
pub fn routes() -> Router {
    Router::new()
        .route("/oagw/v1/proxy/{alias}", axum::routing::any(handle))
        .route("/oagw/v1/proxy/{alias}/{*path}", axum::routing::any(handle))
}

/// The alias and the remainder of the path carried by an inbound proxy request.
///
/// The remainder keeps its leading slash, so `/oagw/v1/proxy/a/v1/x` yields `("a", "/v1/x")` and
/// `/oagw/v1/proxy/a` yields `("a", "")` — exactly the shape the route match compares against.
fn split_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix(PROXY_PREFIX)?;
    let (alias, suffix) = match rest.split_once('/') {
        Some((alias, suffix)) => (alias, format!("/{suffix}")),
        None => (rest, String::new()),
    };
    if alias.is_empty() {
        return None;
    }
    Some((alias.to_string(), suffix))
}

/// Handle one proxied call: HTTP, `text/event-stream` or WebSocket.
async fn handle(
    axum::Extension(state): axum::Extension<ApiState>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let Some((alias, suffix)) = split_path(parts.uri.path()) else {
        return problem_of(
            &DomainError::UpstreamNotFound("a proxy path must name an alias".to_string()),
            None,
            parts.uri.path(),
        );
    };
    let instance = parts.uri.path().to_string();
    if is_preflight(&parts) {
        return preflight(&parts, &instance);
    }
    let tenant = parts
        .extensions
        .get::<SecurityContext>()
        .map_or(super::PUBLIC_TENANT, SecurityContext::subject_tenant_id);
    let subject = parts
        .extensions
        .get::<SecurityContext>()
        .map_or(Uuid::nil(), SecurityContext::subject_id)
        .to_string();
    let remote = forwarded_for(&parts.headers);
    let ancestors = state.ancestors.ancestors(tenant).await;
    let query = parts.uri.query().unwrap_or_default();

    let Some(upstream) = state.data.resolve_alias(tenant, &ancestors, &alias) else {
        return unknown_alias(&alias, &instance);
    };
    if !upstream.enabled {
        return problem_of(
            &DomainError::Disabled(format!("upstream '{alias}' is disabled")),
            None,
            &instance,
        );
    }

    let routes = state.data.routes_for(upstream.id);
    let Some(route) = state.data.match_route(&routes, &parts.method, &suffix) else {
        return problem_of(
            &DomainError::RouteNotFound(format!(
                "no enabled route of upstream '{alias}' matches {} {}",
                parts.method.as_str(),
                suffix
            )),
            None,
            &instance,
        );
    };
    let resolved = match resolve_call(&state, &upstream, route, &parts, &suffix, query, tenant, &ancestors, &alias) {
        Ok(resolved) => resolved,
        Err(error) => return routing_problem(&error, &upstream, &instance),
    };

    if is_websocket(&parts.headers) {
        return super::proxy_ws(&state, parts, resolved).await;
    }

    let body_bytes = match axum::body::to_bytes(body, state.data.settings().max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return problem_of(
                &DomainError::PayloadTooLarge(format!(
                    "request body exceeds {} bytes",
                    state.data.settings().max_body_bytes
                )),
                Some(&resolved),
                &instance,
            );
        }
    };
    if let Err(error) = crate::infra::proxy::service::validate_body(
        &parts.headers,
        body_bytes.len(),
        state.data.settings().max_body_bytes,
    ) {
        return problem_of(&error, Some(&resolved), &instance);
    }

    let security = parts
        .extensions
        .get::<SecurityContext>()
        .cloned()
        .map(std::sync::Arc::new);
    let inbound_origin = origin(&parts.headers).map(str::to_string);
    let inbound = ProxyRequest {
        method: parts.method.clone(),
        path: resolved.path.clone(),
        query: resolved.query.clone(),
        headers: parts.headers,
        body: body_bytes,
        tenant_id: tenant,
        security,
    };

    if let Err(error) = state
        .data
        .check_rate_limit(&resolved, &rate_key(tenant, &subject, &remote, &resolved))
    {
        return problem_of(&error, Some(&resolved), &instance);
    }

    let upstream_call = async {
        let mut outbound = state.data.build_outbound(&resolved, &inbound);
        // ADR-0002 §Execution Order: Auth → Guards → Transform(request) → upstream call.
        state
            .data
            .authenticate(&mut outbound, &resolved)
            .await
            .map_err(map_plugin_error)?;
        state
            .data
            .guard_request(&outbound, &resolved)
            .await
            .map_err(map_plugin_error)?;
        state
            .data
            .transform_request(&mut outbound, &resolved)
            .await
            .map_err(map_plugin_error)?;
        state.data.send(&outbound, &resolved.endpoint).await
    }
    .await;

    let mut response = match upstream_call {
        Ok(response) => response,
        Err(error) => return problem_of(&error, Some(&resolved), &instance),
    };

    if let Some(rules) = resolved
        .upstream
        .headers
        .as_ref()
        .and_then(|h| h.response.as_ref())
    {
        apply_response_rules(&mut response.headers, rules);
    }
    let cors = DataPlane::cors_headers(&resolved, inbound_origin.as_deref());

    match response.body {
        UpstreamBody::Buffered(bytes) => {
            let mut proxy_response = ProxyResponse {
                status: response.status,
                headers: response.headers,
                body: bytes,
            };
            let guarded = async {
                state
                    .data
                    .transform_response(&mut proxy_response, &resolved)
                    .await
                    .map_err(map_plugin_error)?;
                state
                    .data
                    .guard_response(&proxy_response, &resolved)
                    .await
                    .map_err(map_plugin_error)
            };
            if let Err(error) = guarded.await {
                return problem_of(&error, Some(&resolved), &instance);
            }
            let mut built = passthrough(proxy_response, &instance);
            apply_cors(&mut built, &cors);
            built
        }
        UpstreamBody::Streaming(stream) => {
            let proxy_response = ProxyResponse {
                status: response.status,
                headers: response.headers.clone(),
                body: Bytes::new(),
            };
            if let Err(error) = state
                .data
                .guard_response(&proxy_response, &resolved)
                .await
                .map_err(map_plugin_error)
            {
                return problem_of(&error, Some(&resolved), &instance);
            }
            let mut built = Response::builder().status(response.status);
            for (name, value) in &response.headers {
                built = built.header(name, value);
            }
            let mut built = built
                .body(Body::new(stream))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
            problem::stamp_upstream_source(&mut built);
            apply_cors(&mut built, &cors);
            built
        }
    }
}


/// Add the CORS response headers of an allowed cross-origin call.
fn apply_cors(response: &mut Response, headers: &[(String, String)]) {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::try_from(name.as_str()),
            header::HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
}

/// Turn a buffered upstream response into an axum response.
fn passthrough(proxy_response: ProxyResponse, instance: &str) -> Response {
    let mut builder = Response::builder().status(proxy_response.status);
    for (name, value) in &proxy_response.headers {
        builder = builder.header(name, value);
    }
    let mut built = builder
        .body(Body::from(proxy_response.body))
        .unwrap_or_else(|_| {
            problem::problem_response(
                &DomainError::Internal(instance.to_string()),
                &crate::domain::ProblemMeta::new(),
                instance,
            )
        });
    problem::stamp_upstream_source(&mut built);
    built
}

/// The caller address the platform gateway reported, without its port.
///
/// The gear sits behind the platform api-gateway, so the connection's own peer address names the
/// gateway rather than the caller; `X-Forwarded-For` is where the caller's address is carried.
fn forwarded_for(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .map_or_else(String::new, str::to_string)
}

/// Rate-limit scope of a call, per the limit's own sharing mode.
fn rate_key(tenant: Uuid, subject: &str, remote: &str, resolved: &Resolved) -> String {
    let Some(limit) = &resolved.limit else {
        return String::new();
    };
    crate::infra::ratelimit::RateLimiter::scope_key(
        limit,
        tenant,
        resolved.route.as_ref().map_or(Uuid::nil(), |route| route.id),
        subject,
        remote,
    )
}

/// Resolve the endpoint, path, query, CORS check and limits of a call.
///
/// `suffix` is the part of the inbound path after the alias, leading slash included. The part of it
/// that extends beyond the route's own path is what `path_suffix_mode` governs and what gets
/// appended to the route path (DESIGN §Transformation Rules). `tenant`, `ancestors` and `alias`
/// are the caller's chain, so an ancestor that enforces its rate limit can join the merge.
#[allow(clippy::too_many_arguments)]
fn resolve_call(
    state: &ApiState,
    upstream: &crate::domain::model::Upstream,
    route: &crate::domain::model::Route,
    parts: &axum::http::request::Parts,
    suffix: &str,
    query: &str,
    tenant: Uuid,
    ancestors: &[Uuid],
    alias: &str,
) -> Result<Resolved, DomainError> {
    if !route.enabled {
        return Err(DomainError::Disabled(format!("route {} is disabled", route.id)));
    }
    let endpoint = state
        .data
        .select_endpoint(upstream, target_host(&parts.headers).as_deref())?;
    let crate::domain::model::RouteMatch::Http(http) = &route.route_match else {
        return Err(DomainError::RouteNotFound(format!(
            "route {} matches gRPC calls only",
            route.id
        )));
    };
    let prefix = http.path.trim_end_matches('/');
    let appended = suffix.strip_prefix(prefix).unwrap_or_default();
    DataPlane::validate_match(http, &parts.method, appended)?;
    DataPlane::check_query(http, query)?;
    let path = if appended.is_empty() {
        http.path.clone()
    } else if prefix.is_empty() {
        appended.to_string()
    } else {
        format!("{prefix}{appended}")
    };
    let limit = {
        let upstream_limit = upstream
            .rate_limit
            .as_ref()
            .map(crate::infra::ratelimit::effective_limit);
        let route_limit = route
            .rate_limit
            .as_ref()
            .map(crate::infra::ratelimit::effective_limit);
        let enforced = state.data.enforced_ancestor_limits(tenant, ancestors, alias);
        // Stricter always wins, and an ancestor that enforces its limit still applies across
        // shadowing (DESIGN §Hierarchical Configuration).
        let mut merged = upstream_limit;
        for limit in enforced.iter().chain(route_limit.iter()) {
            merged = crate::infra::ratelimit::EffectiveLimit::merge(merged.as_ref(), Some(limit));
        }
        merged
    };
    let resolved = Resolved {
        upstream: upstream.clone(),
        route: Some(route.clone()),
        endpoint,
        path,
        query: DataPlane::filter_query(http, query),
        limit,
    };
    state
        .data
        .check_cors(&resolved, origin(&parts.headers), &parts.method)?;
    Ok(resolved)
}

fn target_host(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-oagw-target-host")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn origin(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
}

/// True when the inbound request is a CORS preflight (ADR-0004).
fn is_preflight(parts: &axum::http::request::Parts) -> bool {
    parts.method == axum::http::Method::OPTIONS
        && parts.headers.contains_key(header::ORIGIN)
        && parts.headers.contains_key("access-control-request-method")
}

/// Answer a CORS preflight locally.
///
/// Browser preflights carry no credentials, so no tenant context is available for upstream
/// resolution: the preflight is answered permissively at the top of the handler and the origin and
/// method are enforced on the actual request instead (ADR-0004 §Preflight Request Handling).
fn preflight(parts: &axum::http::request::Parts, instance: &str) -> Response {
    let Some(origin) = origin(&parts.headers) else {
        return problem_of(
            &DomainError::Validation("a preflight must carry an Origin".to_string()),
            None,
            instance,
        );
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    {
        let headers = response.headers_mut();
        let mut insert = |name: header::HeaderName, value: &str| {
            if let Ok(value) = header::HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        };
        insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        if let Some(method) = parts
            .headers
            .get("access-control-request-method")
            .and_then(|v| v.to_str().ok())
        {
            insert(header::ACCESS_CONTROL_ALLOW_METHODS, method);
        }
        if let Some(requested) = parts
            .headers
            .get("access-control-request-headers")
            .and_then(|v| v.to_str().ok())
        {
            insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
        }
        insert(header::ACCESS_CONTROL_MAX_AGE, "86400");
        insert(
            header::VARY,
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        );
    }
    stamp_gateway_source(&mut response);
    response
}

/// True when the inbound request asks for a WebSocket upgrade.
fn is_websocket(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
        && headers
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

fn problem_of(error: &DomainError, resolved: Option<&Resolved>, instance: &str) -> Response {
    let call_meta = resolved.map_or_else(crate::domain::ProblemMeta::new, |resolved| {
        let mut meta = resolved.meta();
        if matches!(
            error,
            DomainError::TargetHostRequired(_) | DomainError::TargetHostUnknown(_)
        ) {
            meta = meta.with_valid_hosts(valid_hosts(&resolved.upstream.server.endpoints));
        }
        meta
    });
    let mut meta = error.meta_with(call_meta);
    // An error that carries its own machine-readable code keeps it; the rest fall back to the
    // problem type.
    if meta.code.is_none() {
        meta = meta.with_code(error.problem_type());
    }
    problem::problem_response(error, &meta, instance)
}

/// The 404 for an alias that resolved to nothing, naming the alias it was asked about.
fn unknown_alias(alias: &str, instance: &str) -> Response {
    // An alias that resolves to nothing is a route failure at the proxy surface: the DESIGN error
    // table carries a single 404 type (`route.not_found`), and the detail still names the alias.
    let error = DomainError::RouteNotFound(format!("no upstream with alias '{alias}'"));
    let meta = crate::domain::ProblemMeta::new()
        .with_code(error.problem_type())
        .with_alias(alias);
    problem::problem_response(&error, &meta, instance)
}

/// A failure of endpoint selection, which advertises the hosts that *are* configured.
fn routing_problem(
    error: &DomainError,
    upstream: &crate::domain::model::Upstream,
    instance: &str,
) -> Response {
    let mut meta = crate::domain::ProblemMeta::new().with_code(error.problem_type());
    if matches!(
        error,
        DomainError::TargetHostRequired(_)
            | DomainError::TargetHostInvalid(_)
            | DomainError::TargetHostUnknown(_)
    ) {
        meta = meta.with_valid_hosts(valid_hosts(&upstream.server.endpoints));
    }
    problem::problem_response(error, &meta, instance)
}
