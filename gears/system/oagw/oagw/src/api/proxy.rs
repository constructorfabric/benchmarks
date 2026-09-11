// Created: 2026-09-01 by Constructor Tech
//! The proxy handler — the Data Plane's entry point.
//!
//! `docs/DESIGN.md` §3.3 "Proxy API" and §3.2 "Guard Rules" / "Body
//! Validation Rules", plus `docs/ADR/0001-request-routing.md` Appendix A for
//! the `X-OAGW-Target-Host` matrix. Everything before the upstream call
//! happens here: alias resolution down the tenant chain, route matching,
//! endpoint selection, body and query validation, CORS, plugin execution,
//! and the dispatch itself.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::Extension;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use toolkit_security::SecurityContext;

use crate::domain::alias::{self};
use crate::domain::errors::{ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, OagwError, Result};
use crate::domain::hierarchy;
use crate::domain::model::{Cors, PathSuffixMode, Route, Upstream};
use crate::domain::{headers as hdrs, model};
use crate::infra::context::{
    Chain, PluginRequest, PluginResponse, ProxyBody, ProxyOutcome, ProxyResponse,
};
use crate::infra::plugin::executor;

/// `X-OAGW-Target-Host` — the caller's pick of endpoint inside a pool.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Round-robin cursors, one per upstream id.
#[derive(Default)]
pub struct PoolCursor {
    counters: std::collections::HashMap<String, AtomicU64>,
}

impl PoolCursor {
    /// The next index for `upstream_id`, cycling over `len`.
    #[must_use]
    pub fn next(&mut self, upstream_id: &str, len: usize) -> usize {
        let counter = self.counters.entry(upstream_id.to_owned()).or_default();
        let value = counter.fetch_add(1, Ordering::Relaxed);
        if len == 0 {
            0
        } else {
            (value % len as u64) as usize
        }
    }
}

/// Route the request to its upstream.
pub async fn proxy(
    Extension(state): Extension<crate::api::state::OagwState>,
    Extension(ctx): Extension<SecurityContext>,
    request: Request,
) -> Result<Response> {
    // RFC 9457: the `instance` of a gateway error is the request path.
    let path = request.uri().path().to_owned();
    route_request(state, ctx, request)
        .await
        .map_err(|error| error.with_instance(path))
}

async fn route_request(
    state: crate::api::state::OagwState,
    ctx: SecurityContext,
    request: Request,
) -> Result<Response> {
    let (mut parts, body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();
    let upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    let inbound = body.collect().await.map_err(|error| {
        OagwError::validation_error(format!("the request body could not be read: {error}"))
    })?;
    let body = inbound.to_bytes();

    let tenant_id = ctx.subject_tenant_id().to_string();
    let subject = ctx.subject_id().to_string();
    let request_id = hdrs::resolve_request_id(&headers, &crate::domain::new_uuid());

    // 1. CORS preflight: answered permissively at the handler level, before
    //    routing, upstream resolution or any tenant context
    //    (`docs/ADR/0004-cors.md`).
    if method == Method::OPTIONS && is_preflight(&headers) {
        return Ok(preflight(&headers));
    }

    // 2. Alias → upstream, walking the tenant chain descendant-first so a
    //    descendant's definition shadows an ancestor's.
    let (alias, suffix) = split_alias(&proxied_path(
        uri.path(),
        parts
            .extensions
            .get::<axum::extract::MatchedPath>()
            .map(axum::extract::MatchedPath::as_str),
    ));
    let chain = state
        .tenants
        .chain(&ctx, ctx.subject_tenant_id())
        .await
        .unwrap_or_default();
    let resolution = hierarchy::resolve(&state.store, &chain, &alias);
    let effective = hierarchy::effective_upstream(&resolution)?;
    if !effective.enabled {
        return Err(disabled(&effective.alias));
    }
    let route = match_route(&resolution, &effective, &method, &suffix)
        .ok_or_else(|| OagwError::route_not_found(format!("no route matches {method} /{alias}")))?;
    if !route.enabled {
        return Err(OagwError::route_not_found(format!(
            "route '{}' is disabled",
            route.id
        )));
    }
    let merged = merge_upstream(&effective, route);

    // 3. Guard rules that the gateway enforces on its own.
    check_cors(merged.cors.as_ref(), &method, &headers)?;
    validate_body(&headers, body.len(), state.dp.max_body_size())?;
    validate_query(route, uri.query())?;

    // 4. Endpoint selection, honouring `X-OAGW-Target-Host`.
    let target = select_target(&effective, headers.get(TARGET_HOST_HEADER))?;

    // 5. Outbound request, then the plugin chain in phase order.
    let upgrade_requested = is_upgrade(&headers);
    let transform = |map: axum::http::HeaderMap| hdrs::flatten(&map);
    let mut outbound_headers = if upgrade_requested {
        hdrs::transform_upgrade_request(&headers, merged.headers.as_ref())
    } else {
        hdrs::transform_request(&headers, merged.headers.as_ref())
    };
    if let Ok(host) = HeaderValue::from_str(&target.authority()) {
        outbound_headers.insert(axum::http::header::HOST, host);
    }
    outbound_headers.remove(axum::http::header::CONTENT_LENGTH);
    let chain = build_chain(&merged, route);
    let mut request = PluginRequest {
        method: method.as_str().to_owned(),
        path: outbound_path(route, &suffix, uri.query()),
        headers: transform(outbound_headers),
        body: body.to_vec(),
        target,
        alias: alias.clone(),
        upstream_id: merged.id.clone(),
        route_id: Some(route.id.clone()),
        tenant_id: tenant_id.clone(),
        subject: Some(subject),
        request_id: request_id.clone(),
        content_type: headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        auth_config: merged
            .auth
            .as_ref()
            .map(|a| a.config.clone())
            .unwrap_or_default(),
        plugin_config: std::collections::BTreeMap::new(),
        security: ctx.clone(),
    };

    let rate = state.dp.check_rate(merged.rate_limit.as_ref(), &request)?;
    state.dp.check_breaker(&request.target)?;
    state.dp.check_transport(&request.target)?;
    executor::run_request(&state.registries, &mut request, &chain).await?;

    // 6. Dispatch: an upgrade goes to the relay, everything else through the
    //    ordinary forwarder.
    let outcome = if upgrade_requested {
        if upgrade.is_none() {
            return Err(OagwError::protocol_error(
                "the client asked for an upgrade this connection cannot perform",
            ));
        }
        state
            .dp
            .open_websocket(&request, merged.headers.as_ref())
            .await?
    } else {
        let response = state
            .dp
            .send(method.as_str(), &request, merged.headers.as_ref())
            .await;
        match &response {
            Ok(_) => state.dp.record(&request.target, None),
            Err(error) => state.dp.record(&request.target, Some(error)),
        }
        ProxyOutcome::Response(response?)
    };

    match outcome {
        ProxyOutcome::Response(response) => {
            let rendered = render(&state, response, &chain).await?;
            let rendered = with_rate_headers(rendered, rate.as_ref());
            Ok(with_cors_headers(rendered, merged.cors.as_ref(), &headers))
        }
        ProxyOutcome::Upgraded(handle) => relay(handle, upgrade, &state).await,
    }
}

/// Report the limiter's decision on a relayed response (`docs/ADR/0003`).
///
/// An exhausted bucket never reaches this point — `check_rate` has already
/// answered `429` with the same numbers.
fn with_rate_headers(
    mut response: axum::response::Response,
    decision: Option<&crate::domain::ratelimit::Decision>,
) -> axum::response::Response {
    if let Some(decision) = decision {
        for (name, value) in crate::infra::dp::rate_limit_headers(decision) {
            if let Ok(value) = HeaderValue::from_str(&value) {
                response.headers_mut().insert(name, value);
            }
        }
    }
    response
}

/// The response-side plugin phases, then the wire.
async fn render(
    state: &crate::api::state::OagwState,
    response: ProxyResponse,
    chain: &Chain,
) -> Result<Response> {
    let mut plugin_response = PluginResponse {
        status: response.status,
        headers: response.headers,
        plugin_config: std::collections::BTreeMap::new(),
    };
    executor::run_response(&state.registries, &mut plugin_response, chain).await?;
    let status = StatusCode::from_u16(plugin_response.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    // `docs/ADR/0007-error-source-distinction.md`: anything relayed from the
    // upstream — success or error — is marked `upstream`, and the body is
    // passed through unchanged.
    builder = builder.header(ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM);
    for (name, value) in &plugin_response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }
    let body = match response.body {
        ProxyBody::Full(bytes) => axum::body::Body::from(bytes),
        ProxyBody::Stream(stream) => axum::body::Body::new(stream.map_err(axum::Error::new)),
    };
    builder
        .body(body)
        .map_err(|error| OagwError::protocol_error(format!("malformed response: {error}")))
}

/// Hand the negotiated session to the client and splice the two sockets.
///
/// The upstream's `101` head is copied verbatim — its `Sec-WebSocket-Accept`
/// answers the client's own `Sec-WebSocket-Key` — and the byte streams run
/// in both directions until either side hangs up.
async fn relay(
    handle: crate::infra::context::UpgradeHandle,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
    state: &crate::api::state::OagwState,
) -> Result<Response> {
    // The `OnUpgrade` future only resolves once the `101` has been written,
    // so the handshake is answered first and the splice awaited afterwards.
    let upgrade = upgrade
        .ok_or_else(|| OagwError::protocol_error("the client connection cannot be upgraded"))?;
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in &handle.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }
    let response = builder
        .body(axum::body::Body::empty())
        .map_err(|error| OagwError::protocol_error(format!("malformed upgrade: {error}")))?;

    let idle = state.dp.ws_idle_timeout();
    tokio::spawn(async move {
        let upgraded = match upgrade.await {
            Ok(upgraded) => hyper_util::rt::TokioIo::new(upgraded),
            Err(error) => {
                tracing::warn!(error = %error, "client upgrade never completed");
                return;
            }
        };
        let mut client = upgraded;
        let mut upstream = handle.io;
        crate::infra::dp::relay(&mut client, &mut upstream, idle).await;
    });
    Ok(response)
}

/// Split `/alias/rest` into its two halves.
fn split_alias(path: &str) -> (String, String) {
    let trimmed = path.trim_start_matches('/');
    match trimmed.find('/') {
        Some(index) => (trimmed[..index].to_owned(), trimmed[index + 1..].to_owned()),
        None => (trimmed.to_owned(), String::new()),
    }
}

/// The request path as the proxy sees it: from the alias onwards.
///
/// axum hands the handler the original URI, mount prefix included, so the
/// matched pattern — which is the full path even under a nested router — says
/// how many literal segments precede the alias capture.
fn proxied_path(path: &str, matched: Option<&str>) -> String {
    let mount = matched.map_or(0, |pattern| {
        pattern
            .split('/')
            .filter(|segment| !segment.is_empty())
            .take_while(|segment| !segment.starts_with('{'))
            .count()
    });
    let mut rest = path;
    for _ in 0..mount {
        let trimmed = rest.strip_prefix('/').unwrap_or(rest);
        match trimmed.split_once('/') {
            Some((_, remaining)) => rest = remaining,
            None => return String::new(),
        }
    }
    if rest.starts_with('/') {
        rest.to_owned()
    } else {
        format!("/{rest}")
    }
}

/// Pick the endpoint the request goes to.
///
/// `docs/ADR/0001-request-routing.md` "X-OAGW-Target-Host Behavior Matrix":
/// a shared-suffix alias needs the header to disambiguate, an explicit-alias
/// pool round-robins without it.
fn select_target(upstream: &Upstream, requested: Option<&HeaderValue>) -> Result<model::Target> {
    let endpoints = &upstream.server.endpoints;
    if endpoints.is_empty() {
        return Err(OagwError::link_unavailable(format!(
            "upstream '{}' has no endpoints",
            upstream.alias
        )));
    }
    // `X-OAGW-Target-Host` is only *required* when the alias is the common
    // suffix of the pool (ADR 0001's behaviour matrix). An explicit alias on
    // a multi-endpoint pool round-robins instead.
    let shared_suffix = matches!(
        alias::derive_alias(endpoints),
        Some((ref derived, alias::AliasKind::CommonSuffix)) if *derived == upstream.alias
    );
    let target_of = |endpoint: &model::Endpoint| model::Target {
        host: endpoint.host.clone(),
        port: endpoint.port,
        secure: endpoint.is_secure(),
    };

    match (endpoints.len(), shared_suffix, requested) {
        (1, _, requested) => {
            if let Some(requested) = requested {
                let value = requested.to_str().unwrap_or_default();
                if !endpoints
                    .iter()
                    .any(|e| e.host.eq_ignore_ascii_case(value.trim()))
                {
                    return Err(OagwError::unknown_target_host(value));
                }
            }
            Ok(target_of(&endpoints[0]))
        }
        (_, _, None) if shared_suffix => Err(OagwError::missing_target_host(&upstream.alias)),
        (_, _, requested) => {
            let requested = requested.and_then(|v| v.to_str().ok()).map(str::trim);
            match requested {
                Some(value) => endpoints
                    .iter()
                    .find(|e| e.host.eq_ignore_ascii_case(value))
                    .map(target_of)
                    .ok_or_else(|| OagwError::unknown_target_host(value)),
                None => Ok(target_of(&endpoints[0])),
            }
        }
    }
}

/// The documented default body checks.
fn validate_body(headers: &HeaderMap, actual: usize, limit: usize) -> Result<()> {
    if let Some(value) = headers.get(axum::http::header::CONTENT_LENGTH) {
        let text = value.to_str().unwrap_or_default();
        let declared: usize = text
            .parse()
            .map_err(|_| OagwError::validation_error("content-length is not a valid integer"))?;
        if declared != actual {
            return Err(OagwError::validation_error(format!(
                "content-length {declared} does not match the {actual} bytes received"
            )));
        }
    }
    if actual > limit {
        return Err(OagwError::payload_too_large(limit));
    }
    if let Some(value) = headers.get(axum::http::header::TRANSFER_ENCODING) {
        let encodings = value.to_str().unwrap_or_default().to_ascii_lowercase();
        let odd = encodings
            .split(',')
            .map(str::trim)
            .find(|e| !e.is_empty() && *e != "chunked");
        if let Some(encoding) = odd {
            return Err(OagwError::validation_error(format!(
                "transfer-encoding '{encoding}' is not supported; only 'chunked' is"
            )));
        }
    }
    Ok(())
}

/// Reject query parameters the route does not allow.
fn validate_query(route: &Route, query: Option<&str>) -> Result<()> {
    let Some(http) = route.matcher.http.as_ref() else {
        return Ok(());
    };
    if http.query_allowlist.is_empty() {
        if query.is_some_and(|q| !q.is_empty()) {
            return Err(OagwError::validation_error(
                "this route does not accept query parameters",
            ));
        }
        return Ok(());
    }
    let Some(query) = query else {
        return Ok(());
    };
    for (name, _) in form_urlencoded::parse(query.as_bytes()) {
        if !http
            .query_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&name))
        {
            return Err(OagwError::validation_error(format!(
                "query parameter '{name}' is not allowed by this route"
            )));
        }
    }
    Ok(())
}

/// `true` when the client asked for a WebSocket upgrade.
fn is_upgrade(headers: &HeaderMap) -> bool {
    let connection = headers
        .get(axum::http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let upgrade = headers
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    connection.split(',').any(|t| t.trim() == "upgrade") && upgrade.contains("websocket")
}

/// A permissive `204` preflight answer, no upstream resolution involved.
/// `true` for a browser preflight: an `OPTIONS` carrying both `Origin` and the
/// method it asks to run.
fn is_preflight(headers: &HeaderMap) -> bool {
    headers.contains_key(axum::http::header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

/// The permissive preflight answer, echoing what the browser asked for.
///
/// `docs/ADR/0004-cors.md` "Preflight Request Handling": no upstream
/// resolution and no tenant context — origin and method checks are deferred
/// to the actual request that follows.
fn preflight(headers: &HeaderMap) -> Response {
    let mut response = (StatusCode::NO_CONTENT, String::new()).into_response();
    let out = response.headers_mut();
    for (name, value) in [
        (
            "access-control-allow-origin",
            headers.get(axum::http::header::ORIGIN),
        ),
        (
            "access-control-allow-methods",
            headers.get("access-control-request-method"),
        ),
        (
            "access-control-allow-headers",
            headers.get("access-control-request-headers"),
        ),
    ] {
        if let Some(value) = value
            && let Ok(value) = HeaderValue::from_bytes(value.as_bytes())
        {
            out.insert(name, value);
        }
    }
    out.insert("access-control-max-age", HeaderValue::from_static("86400"));
    out.insert(
        "vary",
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    response
}

/// Validate a cross-origin actual request against the effective policy.
///
/// `docs/ADR/0004-cors.md` "Actual Request Handling": origin first, then
/// method, both rejected with 403 before anything reaches the upstream.
fn check_cors(cors: Option<&Cors>, method: &Method, headers: &HeaderMap) -> Result<()> {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return Ok(());
    };
    let origin = origin.to_str().unwrap_or_default();
    let Some(cors) = cors.filter(|cors| cors.enabled) else {
        return Ok(());
    };
    if !cors.allows_origin(origin) {
        return Err(OagwError::cors_origin_not_allowed(origin));
    }
    if !cors
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(OagwError::cors_method_not_allowed(method.as_str()));
    }
    Ok(())
}

/// The CORS headers a successful cross-origin response carries.
fn cors_response_headers(cors: Option<&Cors>, headers: &HeaderMap) -> Vec<(&'static str, String)> {
    let Some(cors) = cors.filter(|cors| cors.enabled) else {
        return Vec::new();
    };
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return Vec::new();
    };
    let origin = origin.to_str().unwrap_or_default();
    if !cors.allows_origin(origin) {
        return Vec::new();
    }
    let mut out = vec![("access-control-allow-origin", origin.to_owned())];
    if !cors.expose_headers.is_empty() {
        out.push((
            "access-control-expose-headers",
            cors.expose_headers.join(", "),
        ));
    }
    if cors.allow_credentials {
        out.push(("access-control-allow-credentials", "true".to_owned()));
    }
    out.push(("vary", "Origin".to_owned()));
    out
}

/// Attach the CORS headers a successful cross-origin response carries.
fn with_cors_headers(
    mut response: axum::response::Response,
    cors: Option<&Cors>,
    headers: &HeaderMap,
) -> axum::response::Response {
    for (name, value) in cors_response_headers(cors, headers) {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// The path the upstream sees: the route's prefix, then the suffix.
fn outbound_path(route: &Route, suffix: &str, query: Option<&str>) -> String {
    let Some(http) = route.matcher.http.as_ref() else {
        return format!("/{suffix}");
    };
    let mut path = http.path.clone();
    if http.path_suffix_mode == PathSuffixMode::Append {
        let extra = unmatched_suffix(&http.path, suffix);
        if !extra.is_empty() {
            if !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(extra);
        }
    }
    match query {
        Some(query) if !query.is_empty() => format!("{path}?{query}"),
        _ => path,
    }
}

/// The part of the proxy path the route's own pattern does not already cover.
///
/// `/oagw/v1/proxy/{alias}/v1/chat/completions` against a route whose path is
/// `/v1/chat` appends `completions`, so the upstream sees the path the caller
/// asked for rather than the pattern twice over.
fn unmatched_suffix<'a>(pattern: &str, suffix: &'a str) -> &'a str {
    let pattern = pattern.trim_matches('/');
    if pattern.is_empty() {
        return suffix;
    }
    suffix
        .strip_prefix(pattern)
        .unwrap_or(suffix)
        .trim_start_matches('/')
}

fn disabled(alias: &str) -> OagwError {
    OagwError::link_unavailable(format!("upstream '{alias}' is disabled"))
}

/// Flatten upstream-then-route policy into one set of rules.
///
/// Routes override the upstream rate limit and CORS blocks wholesale, and
/// the gateway never lets a route relax an `enforce`-mode ancestor limit —
/// [`crate::domain::merge_rate_limits`] keeps whichever side is stricter.
fn merge_upstream(upstream: &Upstream, route: &Route) -> Upstream {
    let mut merged = upstream.clone();
    if route.rate_limit.is_some() || upstream.rate_limit.is_some() {
        merged.rate_limit = match (&upstream.rate_limit, &route.rate_limit) {
            (Some(a), Some(b)) => Some(crate::domain::merge_rate_limits(a, b)),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };
    }
    if route.cors.is_some() {
        merged.cors = route.cors.clone();
    }
    merged
}

/// Auth → guards → transforms, upstream plugins ahead of route plugins.
///
/// Auth is stated one of two ways: the upstream's `auth` block, which names
/// the plugin and carries its configuration, or an auth plugin bound in
/// `plugins.items`. The block is the one `docs/DESIGN.md` §3.2 describes and
/// wins when both are present; the bound plugin is otherwise the first auth
/// reference upstream or route lists. Dropping it would forward traffic the
/// operator asked to have authenticated, so it is never ignored — a reference
/// with no backing implementation fails the request instead.
fn build_chain(upstream: &Upstream, route: &Route) -> Chain {
    let mut chain = Chain::empty();
    if let Some(auth) = &upstream.auth
        && let Some(id) = &auth.auth_type
    {
        chain.auth = Some(model::PluginBinding::Bound {
            plugin_ref: id.clone(),
            config: auth.config.clone(),
        });
    }
    let bound = upstream
        .plugins
        .items
        .iter()
        .chain(&route.plugins.items)
        .collect::<Vec<_>>();
    if chain.auth.is_none() {
        chain.auth = bound
            .iter()
            .find(|binding| {
                matches!(
                    crate::domain::split_plugin_kind(binding.id()),
                    Some(("auth", _))
                )
            })
            .map(|binding| (*binding).clone());
    }
    for binding in &bound {
        match crate::domain::split_plugin_kind(binding.id()) {
            Some(("guard", _)) => chain.guards.push((*binding).clone()),
            Some(("transform", _)) => chain.transforms.push((*binding).clone()),
            _ => {}
        }
    }
    chain
}

/// Longest-prefix route match, descendant routes first.
fn match_route<'a>(
    resolution: &'a hierarchy::Resolution,
    upstream: &Upstream,
    method: &Method,
    suffix: &str,
) -> Option<&'a Route> {
    let levels =
        std::iter::once(&resolution.levels[resolution.selected]).chain(resolution.ancestors());
    for level in levels {
        let candidates = level
            .routes
            .iter()
            .filter(|r| r.enabled && r.upstream_id == upstream.id);
        let mut best: Option<&Route> = None;
        for route in candidates {
            let Some(http) = route.matcher.http.as_ref() else {
                continue;
            };
            if !http
                .methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method.as_str()))
            {
                continue;
            }
            if http.path_suffix_mode == PathSuffixMode::Disabled && !suffix.is_empty() {
                continue;
            }
            if !prefix_matches(&http.path, suffix) {
                continue;
            }
            let better = best.is_none_or(|current| {
                http.path.len() > current.matcher.http.as_ref().map_or(0, |h| h.path.len())
                    || (http.path.len()
                        == current.matcher.http.as_ref().map_or(0, |h| h.path.len())
                        && route.priority > current.priority)
            });
            if better {
                best = Some(route);
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

/// `true` when `suffix` falls under `pattern`.
fn prefix_matches(pattern: &str, suffix: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');
    if pattern.is_empty() || pattern == "/" {
        return true;
    }
    if suffix.is_empty() {
        return false;
    }
    suffix == pattern.trim_start_matches('/')
        || suffix.starts_with(&format!("{}/", pattern.trim_start_matches('/')))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_alias_and_its_suffix_are_split() {
        assert_eq!(
            split_alias("/api.openai.com/v1/x"),
            ("api.openai.com".to_owned(), "v1/x".to_owned())
        );
        assert_eq!(split_alias("/solo"), ("solo".to_owned(), String::new()));
        assert_eq!(split_alias("/a/b/c/"), ("a".to_owned(), "b/c/".to_owned()));
    }

    #[test]
    fn a_relayed_response_reports_the_limiter_decision() {
        let decision = crate::domain::ratelimit::Decision {
            allowed: true,
            remaining: 4,
            retry_after_secs: 0,
            reset_secs: 12,
            limit: 5,
        };
        let response = with_rate_headers(
            axum::response::Response::builder()
                .status(200)
                .body(axum::body::Body::empty())
                .expect("static response"),
            Some(&decision),
        );
        let headers = response.headers();
        assert_eq!(
            headers.get("x-ratelimit-limit"),
            Some(&"5".parse().unwrap())
        );
        assert_eq!(
            headers.get("x-ratelimit-remaining"),
            Some(&"4".parse().unwrap())
        );
        assert_eq!(
            headers.get("x-ratelimit-reset"),
            Some(&"12".parse().unwrap())
        );
    }

    #[test]
    fn an_unlimited_response_carries_no_rate_headers() {
        let response = with_rate_headers(
            axum::response::Response::builder()
                .status(200)
                .body(axum::body::Body::empty())
                .expect("static body"),
            None,
        );
        assert!(response.headers().get("x-ratelimit-limit").is_none());
    }

    #[test]
    fn the_mount_prefix_is_stripped_from_the_request_path() {
        assert_eq!(
            proxied_path(
                "/oagw/v1/proxy/local-echo/v1/hello",
                Some("/oagw/v1/proxy/{alias}/{*suffix}")
            ),
            "/local-echo/v1/hello"
        );
        assert_eq!(
            proxied_path("/oagw/v1/proxy/local-echo", Some("/oagw/v1/proxy/{alias}")),
            "/local-echo"
        );
        // No matched pattern: the path is taken as given.
        assert_eq!(
            proxied_path("/local-echo/v1/hello", None),
            "/local-echo/v1/hello"
        );
        assert_eq!(
            proxied_path("/oagw/v1/proxy", Some("/oagw/v1/proxy/{alias}")),
            ""
        );
    }

    #[test]
    fn a_prefix_matches_itself_and_its_children() {
        assert!(prefix_matches("/v1/chat", "v1/chat"));
        assert!(prefix_matches("/v1/chat", "v1/chat/completions"));
        assert!(!prefix_matches("/v1/chat", "v1/chatx"));
        assert!(prefix_matches("/", "anything"));
        assert!(!prefix_matches("/v1", ""));
    }

    #[test]
    fn the_suffix_is_appended_to_the_route_path() {
        let route = Route {
            matcher: model::RouteMatch {
                http: Some(model::HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/v1/chat".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            ..Route::default()
        };
        assert_eq!(
            outbound_path(&route, "v1/chat/completions", Some("model=gpt")),
            "/v1/chat/completions?model=gpt"
        );
        assert_eq!(outbound_path(&route, "v1/chat", None), "/v1/chat");
    }

    #[test]
    fn the_route_path_is_not_repeated_in_the_outbound_path() {
        let route = Route {
            matcher: model::RouteMatch {
                http: Some(model::HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/v1".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            ..Route::default()
        };
        assert_eq!(outbound_path(&route, "v1/hello", None), "/v1/hello");
        assert_eq!(outbound_path(&route, "v1", None), "/v1");
        let catch_all = Route {
            matcher: model::RouteMatch {
                http: Some(model::HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            ..Route::default()
        };
        assert_eq!(outbound_path(&catch_all, "v1/hello", None), "/v1/hello");
    }

    #[test]
    fn a_disabled_suffix_mode_leaves_the_path_alone() {
        let route = Route {
            matcher: model::RouteMatch {
                http: Some(model::HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/v1/chat".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Disabled,
                }),
                grpc: None,
            },
            ..Route::default()
        };
        assert_eq!(outbound_path(&route, "completions", None), "/v1/chat");
    }

    #[test]
    fn a_content_length_must_match_the_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_LENGTH,
            HeaderValue::from_static("3"),
        );
        assert!(validate_body(&headers, 3, 100).is_ok());
        let err = validate_body(&headers, 4, 100).unwrap_err();
        assert_eq!(err.status_value(), 400, "{err}");
    }

    #[test]
    fn an_oversized_body_is_refused() {
        let headers = HeaderMap::new();
        let err = validate_body(&headers, 101, 100).unwrap_err();
        assert_eq!(err.status_value(), 413, "{err}");
    }

    #[test]
    fn a_transfer_encoding_other_than_chunked_is_refused() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::TRANSFER_ENCODING,
            HeaderValue::from_static("gzip"),
        );
        let err = validate_body(&headers, 0, 100).unwrap_err();
        assert_eq!(err.status_value(), 400, "{err}");
        headers.insert(
            axum::http::header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        assert!(validate_body(&headers, 0, 100).is_ok());
    }

    fn route(allowlist: &[&str]) -> Route {
        Route {
            matcher: model::RouteMatch {
                http: Some(model::HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/".to_owned(),
                    query_allowlist: allowlist.iter().map(|s| (*s).to_owned()).collect(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            ..Route::default()
        }
    }

    #[test]
    fn an_empty_allowlist_admits_no_query() {
        let r = route(&[]);
        assert!(validate_query(&r, None).is_ok());
        assert!(validate_query(&r, Some("")).is_ok());
        let err = validate_query(&r, Some("a=1")).unwrap_err();
        assert_eq!(err.status_value(), 400, "{err}");
    }

    fn bound(id: &str) -> model::PluginBinding {
        model::PluginBinding::Ref(id.to_owned())
    }

    fn plugin_set(items: &[model::PluginBinding]) -> model::PluginSet {
        model::PluginSet {
            sharing: model::SharingMode::Private,
            items: items.to_vec(),
        }
    }

    #[test]
    fn the_auth_block_supplies_the_auth_stage() {
        let mut upstream = pooled(&["api.openai.com"]);
        upstream.auth = Some(model::AuthConfig {
            sharing: model::SharingMode::Private,
            auth_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
            config: [("header".to_owned(), serde_json::json!("x-key"))].into(),
        });
        let chain = build_chain(&upstream, &route(&[]));
        assert_eq!(
            chain.auth.as_ref().map(model::PluginBinding::id),
            Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
        );
        assert!(chain.guards.is_empty());
    }

    #[test]
    fn a_bound_auth_plugin_runs_when_the_auth_block_is_absent() {
        let mut upstream = pooled(&["api.openai.com"]);
        upstream.plugins = plugin_set(&[bound(
            "gts.cf.core.oagw.auth_plugin.v1~11111111-1111-4111-8111-111111111111",
        )]);
        let chain = build_chain(&upstream, &route(&[]));
        assert_eq!(
            chain.auth.as_ref().map(model::PluginBinding::id),
            Some("gts.cf.core.oagw.auth_plugin.v1~11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn the_auth_block_beats_a_bound_auth_plugin() {
        let mut upstream = pooled(&["api.openai.com"]);
        upstream.auth = Some(model::AuthConfig {
            sharing: model::SharingMode::Private,
            auth_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
            config: Default::default(),
        });
        upstream.plugins = plugin_set(&[bound(
            "gts.cf.core.oagw.auth_plugin.v1~22222222-2222-4222-8222-222222222222",
        )]);
        let chain = build_chain(&upstream, &route(&[]));
        assert_eq!(
            chain.auth.as_ref().map(model::PluginBinding::id),
            Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
        );
    }

    #[test]
    fn guards_and_transforms_are_sorted_into_their_stages() {
        let mut upstream = pooled(&["api.openai.com"]);
        upstream.plugins = plugin_set(&[
            bound("gts.cf.core.oagw.guard_plugin.v1~33333333-3333-4333-8333-333333333333"),
            bound("gts.cf.core.oagw.transform_plugin.v1~44444444-4444-4444-8444-444444444444"),
            bound("gts.cf.core.oagw.auth_plugin.v1~55555555-5555-4555-8555-555555555555"),
        ]);
        let mut r = route(&[]);
        r.plugins = plugin_set(&[bound(
            "gts.cf.core.oagw.guard_plugin.v1~66666666-6666-4666-8666-666666666666",
        )]);
        let chain = build_chain(&upstream, &r);
        assert_eq!(
            chain.auth.as_ref().map(model::PluginBinding::id),
            Some("gts.cf.core.oagw.auth_plugin.v1~55555555-5555-4555-8555-555555555555")
        );
        assert_eq!(
            chain
                .guards
                .iter()
                .map(model::PluginBinding::id)
                .collect::<Vec<_>>(),
            vec![
                "gts.cf.core.oagw.guard_plugin.v1~33333333-3333-4333-8333-333333333333",
                "gts.cf.core.oagw.guard_plugin.v1~66666666-6666-4666-8666-666666666666",
            ]
        );
        assert_eq!(chain.transforms.len(), 1);
    }

    #[test]
    fn the_allowlist_is_enforced_case_insensitively() {
        let r = route(&["api-version"]);
        assert!(validate_query(&r, Some("Api-Version=1")).is_ok());
        let err = validate_query(&r, Some("other=1")).unwrap_err();
        assert_eq!(err.status_value(), 400, "{err}");
    }

    fn pooled(hosts: &[&str]) -> Upstream {
        Upstream {
            alias: "vendor.com".to_owned(),
            server: model::ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|h| model::Endpoint {
                        scheme: "https".to_owned(),
                        host: (*h).to_owned(),
                        port: 443,
                    })
                    .collect(),
            },
            ..Upstream::default()
        }
    }

    #[test]
    fn a_single_endpoint_never_needs_the_header() {
        let upstream = pooled(&["api.openai.com"]);
        assert_eq!(
            select_target(&upstream, None).expect("target").host,
            "api.openai.com"
        );
    }

    fn cors() -> Cors {
        Cors {
            sharing: model::SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            allow_credentials: true,
            ..Cors::default()
        }
    }

    fn request_headers(entries: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    #[test]
    fn a_preflight_is_answered_from_the_request_alone() {
        let headers = request_headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            (
                "access-control-request-headers",
                "Content-Type, Authorization",
            ),
        ]);
        let response = preflight(&headers);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let out = response.headers();
        assert_eq!(
            out.get("access-control-allow-origin").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(out.get("access-control-allow-methods").unwrap(), "POST");
        assert_eq!(
            out.get("access-control-allow-headers").unwrap(),
            "Content-Type, Authorization"
        );
        assert_eq!(out.get("access-control-max-age").unwrap(), "86400");
        assert!(
            out.get("vary")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("Origin")
        );
    }

    #[test]
    fn only_a_preflight_is_detected_as_one() {
        assert!(!is_preflight(&request_headers(&[(
            "origin",
            "https://app.example.com"
        )])));
        assert!(!is_preflight(&request_headers(&[(
            "access-control-request-method",
            "POST"
        )])));
    }

    #[test]
    fn a_same_origin_request_skips_cors_checks() {
        let headers = request_headers(&[("origin", "https://evil.com")]);
        assert!(check_cors(None, &Method::GET, &headers).is_ok());
        let mut disabled = cors();
        disabled.enabled = false;
        assert!(check_cors(Some(&disabled), &Method::GET, &headers).is_ok());
    }

    #[test]
    fn a_disallowed_origin_is_rejected_before_forwarding() {
        let headers = request_headers(&[("origin", "https://evil.com")]);
        let err = check_cors(Some(&cors()), &Method::GET, &headers).unwrap_err();
        assert_eq!(err.status_value(), 403, "{err}");
        assert_eq!(
            err.type_uri(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert_eq!(
            err.detail(),
            "origin 'https://evil.com' not in allowed origins list"
        );
    }

    #[test]
    fn a_disallowed_method_is_rejected_before_forwarding() {
        let headers = request_headers(&[("origin", "https://app.example.com")]);
        let err = check_cors(Some(&cors()), &Method::DELETE, &headers).unwrap_err();
        assert_eq!(err.status_value(), 403, "{err}");
        assert_eq!(
            err.type_uri(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );
    }

    #[test]
    fn an_allowed_request_passes_and_carries_the_cors_headers() {
        let headers = request_headers(&[("origin", "https://app.example.com")]);
        assert!(check_cors(Some(&cors()), &Method::POST, &headers).is_ok());
        let response = axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .expect("response");
        let out = with_cors_headers(response, Some(&cors()), &headers)
            .headers()
            .clone();
        assert_eq!(
            out.get("access-control-allow-origin").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(out.get("access-control-allow-credentials").unwrap(), "true");
        assert_eq!(out.get("vary").unwrap(), "Origin");
    }

    #[test]
    fn a_disallowed_origin_gets_no_cors_response_headers() {
        let headers = request_headers(&[("origin", "https://evil.com")]);
        let response = axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .expect("response");
        let out = with_cors_headers(response, Some(&cors()), &headers)
            .headers()
            .clone();
        assert!(out.get("access-control-allow-origin").is_none());
    }

    #[test]
    fn a_shared_suffix_alias_requires_the_header() {
        let upstream = pooled(&["us.vendor.com", "eu.vendor.com"]);
        let err = select_target(&upstream, None).unwrap_err();
        assert_eq!(err.status_value(), 400, "{err}");
        assert_eq!(
            err.type_uri(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
    }

    #[test]
    fn an_unknown_target_host_is_rejected() {
        let upstream = pooled(&["us.vendor.com", "eu.vendor.com"]);
        let value = HeaderValue::from_static("ap.vendor.com");
        let err = select_target(&upstream, Some(&value)).unwrap_err();
        assert_eq!(err.status_value(), 400, "{err}");
    }

    #[test]
    fn a_known_target_host_selects_its_endpoint() {
        let upstream = pooled(&["us.vendor.com", "eu.vendor.com"]);
        let value = HeaderValue::from_static("eu.vendor.com");
        assert_eq!(
            select_target(&upstream, Some(&value)).expect("target").host,
            "eu.vendor.com"
        );
    }

    #[test]
    fn an_explicit_alias_pool_round_robins_without_the_header() {
        let mut upstream = pooled(&["a.example.com", "b.example.com"]);
        upstream.alias = "my-service".to_owned();
        let first = select_target(&upstream, None).expect("target");
        assert!(["a.example.com", "b.example.com"].contains(&first.host.as_str()));
    }

    #[test]
    fn the_single_endpoint_header_is_still_validated() {
        let upstream = pooled(&["api.openai.com"]);
        let err =
            select_target(&upstream, Some(&HeaderValue::from_static("other.com"))).unwrap_err();
        assert_eq!(err.status_value(), 400, "{err}");
    }
}
