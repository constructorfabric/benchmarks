//! The data plane: `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`.
//!
//! One request travels through, in order: CORS preflight short-circuit, body
//! validation, alias + route + endpoint resolution, rate limiting, the CORS
//! origin/method check, the plugin chain (auth → guards → transforms), the
//! outbound hop, and the response transformations. Every response that leaves
//! here carries [`crate::ids::ERROR_SOURCE_HEADER`]: `gateway` for a problem
//! this gateway produced, `upstream` for anything the upstream answered with —
//! including its errors (ADR 0007).

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use toolkit_security::SecurityContext;

use super::{ApiState, problem, trace_id_of};
use crate::domain::model::{CorsRule, Passthrough};
use crate::domain::plugin::ProxyContext;
use crate::domain::query::OutboundQuery;
use crate::domain::service::ResolvedRequest;
use crate::error::{DomainError, ErrorKind};
use crate::ids;

/// Mount point of the data plane; the alias and the suffix follow it.
const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// Headers consumed by the gateway or by the HTTP protocol itself; never
/// forwarded (DESIGN §3.2, Headers Transformation).
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// `Access-Control-Max-Age` of a preflight response, in seconds (ADR 0004).
const PREFLIGHT_MAX_AGE: &str = "86400";

/// `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
pub async fn proxy(
    uri: Uri,
    method: Method,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    request: axum::extract::Request,
) -> Response {
    let trace_id = trace_id_of(request.headers());
    let Some((alias, path_suffix)) = split_proxy_path(uri.path()) else {
        return problem(
            DomainError::new(
                ErrorKind::Validation,
                "the proxy URL does not name an alias",
            ),
            &uri,
            trace_id,
        );
    };
    execute(&state, security, method, uri, alias, path_suffix, request).await
}

/// Split the proxy path into its alias and optional suffix, both verbatim.
///
/// The suffix is *not* percent-decoded: it is spliced into the outbound path
/// exactly as the client wrote it. A query string is dropped — it is handled
/// separately, from `Uri::query()`, and never reaches the suffix.
fn split_proxy_path(path: &str) -> Option<(String, Option<String>)> {
    let path = path.split('?').next().unwrap_or(path);
    let rest = path.strip_prefix(PROXY_PREFIX)?;
    let (alias, suffix) = match rest.split_once('/') {
        Some((alias, suffix)) => (alias, Some(suffix)),
        None => (rest, None),
    };
    if alias.is_empty() {
        return None;
    }
    Some((
        alias.to_owned(),
        suffix
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_owned),
    ))
}

/// Drive one proxied request end to end.
#[allow(clippy::too_many_lines)]
async fn execute(
    state: &ApiState,
    security: SecurityContext,
    method: Method,
    uri: Uri,
    alias: String,
    path_suffix: Option<String>,
    request: axum::extract::Request,
) -> Response {
    let inbound = request.headers().clone();
    let trace_id = trace_id_of(&inbound);

    // A browser preflight is answered locally: it carries no credentials, so
    // there is no tenant to resolve the alias against (ADR 0004).
    if is_preflight(&method, &inbound) {
        return preflight_response(&inbound);
    }

    // `hyper::upgrade::on` consumes the extension, so it is taken before the
    // request is taken apart.
    let is_websocket = is_websocket_upgrade(&inbound);
    let client_ip = client_ip_of(&request);
    let mut request = request;
    let client_upgrade = is_websocket.then(|| hyper::upgrade::on(&mut request));

    let body = match read_body(request.into_body(), &inbound, &state.service.config).await {
        Ok(body) => body,
        Err(err) => return problem(err, &uri, trace_id),
    };

    let resolved = match state
        .service
        .resolve_proxy(
            &security,
            &alias,
            path_suffix.as_deref(),
            &method,
            uri.query(),
            &client_ip,
            target_host(&inbound),
        )
        .await
    {
        Ok(resolved) => resolved,
        Err(err) => return problem(err, &uri, trace_id),
    };

    if let Err(err) = state.service.check_rate_limit(
        &resolved,
        security.subject_tenant_id(),
        security.subject_id(),
        &client_ip,
        crate::domain::ratelimit::now_ms(),
    ) {
        return problem(err, &uri, trace_id);
    }

    if let Err(err) = check_cors(&resolved, &method, &inbound) {
        return problem(err, &uri, trace_id);
    }

    let mut draft = match build_outbound_request(&resolved, uri.query(), &inbound, body) {
        Ok(draft) => draft,
        Err(err) => return problem(err, &uri, trace_id),
    };

    if let Err(err) =
        run_request_plugins(state, &security, &resolved, &method, &inbound, &mut draft).await
    {
        return problem(err, &uri, trace_id);
    }

    let request = match finalize_outbound(&resolved.outbound_path, &method, draft) {
        Ok(request) => request,
        Err(err) => return problem(err, &uri, trace_id),
    };

    if is_websocket {
        return websocket_response(
            state,
            &security,
            &resolved,
            &uri,
            trace_id,
            &inbound,
            &request,
            client_upgrade,
        )
        .await;
    }

    // ---- the outbound hop --------------------------------------------------
    let mut connection = match state.outbound.connect(&resolved.endpoint).await {
        Ok(connection) => connection,
        Err(err) => return problem(err, &uri, trace_id),
    };
    let send = tokio::time::timeout(state.outbound.request_timeout(), connection.send(request));
    let upstream = match send.await {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(err)) => return problem(err, &uri, trace_id),
        Err(_) => return problem(timeout_of(&resolved), &uri, trace_id),
    };

    upstream_response(
        state, &security, &resolved, &uri, trace_id, &inbound, upstream,
    )
    .await
}

/// The client IP the rate limiter keys an `ip`-scoped bucket on.
fn client_ip_of(request: &axum::extract::Request) -> String {
    request
        .extensions()
        .get::<std::net::SocketAddr>()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Read and validate the request body.
///
/// # Errors
///
/// [`ErrorKind::PayloadTooLarge`] beyond the configured ceiling,
/// [`ErrorKind::Validation`] for a `Content-Length` that is not a number,
/// disagrees with the body, or a `Transfer-Encoding` other than `chunked`.
async fn read_body(
    body: Body,
    headers: &HeaderMap,
    config: &crate::config::OagwConfig,
) -> Result<Bytes, DomainError> {
    if let Some(encoding) = headers
        .get(header::TRANSFER_ENCODING)
        .and_then(|v| v.to_str().ok())
        && !encoding
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        {
            return Err(validation(format!(
                "Transfer-Encoding {encoding:?} is not supported; only `chunked` is"
            )));
        }
    let declared = match headers.get(header::CONTENT_LENGTH) {
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| validation("`Content-Length` is not a valid integer"))?
                .trim()
                .parse::<usize>()
                .map_err(|_| validation("`Content-Length` is not a valid integer"))?,
        ),
        None => None,
    };
    let collected = axum::body::to_bytes(body, config.max_body_size_bytes)
        .await
        .map_err(|_| {
            DomainError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "the request body exceeds the {} byte limit",
                    config.max_body_size_bytes
                ),
            )
        })?;
    if let Some(declared) = declared
        && declared != collected.len() {
            return Err(validation(format!(
                "`Content-Length` is {declared} but the body is {} bytes",
                collected.len()
            )));
        }
    Ok(collected)
}

/// The `X-OAGW-Target-Host` header of a request, if it carried one.
fn target_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(ids::TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
}

/// `true` for `OPTIONS` + `Origin` + `Access-Control-Request-Method`.
fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

/// Answer a preflight permissively: the actual request is where the origin and
/// the method are enforced (ADR 0004).
fn preflight_response(headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let cors = response.headers_mut();
    if let Some(origin) = headers.get(header::ORIGIN) {
        cors.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = headers.get("access-control-request-method") {
        cors.insert(header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(request_headers) = headers.get("access-control-request-headers") {
        cors.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            request_headers.clone(),
        );
    }
    if let Ok(value) = HeaderValue::from_str(PREFLIGHT_MAX_AGE) {
        cors.insert(header::ACCESS_CONTROL_MAX_AGE, value);
    }
    if let Ok(value) = HeaderValue::from_str(
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
    ) {
        cors.insert(header::VARY, value);
    }
    cors.insert(
        HeaderName::from_static(ids::ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ids::ERROR_SOURCE_GATEWAY),
    );
    response
}

/// `true` when the request asks to switch protocols to WebSocket.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("websocket"))
        });
    let connection = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        });
    upgrade && connection
}

/// The outbound request as it accumulates through the plugin chain.
///
/// The URI is only assembled once the chain is done: an auth plugin may add a
/// query parameter.
struct OutboundDraft {
    headers: HeaderMap,
    body: Vec<u8>,
    query: OutboundQuery,
}

/// Build the outbound request: passthrough, hop-by-hop strip, then the
/// upstream's header rules.
///
/// # Errors
///
/// [`ErrorKind::Validation`] when the endpoint host cannot be sent.
fn build_outbound_request(
    resolved: &ResolvedRequest,
    raw_query: Option<&str>,
    inbound: &HeaderMap,
    body: Bytes,
) -> Result<OutboundDraft, DomainError> {
    let websocket = is_websocket_upgrade(inbound);
    let mut headers = passthrough_headers(resolved, inbound);

    // Well-known headers the gateway must set or adjust itself (DESIGN §3.2):
    // the body is forwarded as received, so its type travels with it.
    if let Some(content_type) = inbound.get(header::CONTENT_TYPE) {
        headers.insert(header::CONTENT_TYPE, content_type.clone());
    } else if !body.is_empty() {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
    }
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&authority_of(&resolved.endpoint)).map_err(|_| {
            validation(format!(
                "endpoint host {:?} cannot be sent as a Host header",
                resolved.endpoint.host
            ))
        })?,
    );
    if websocket {
        // Stripped above as hop-by-hop and re-added here: this *is* the
        // protocol switch, not header leakage.
        for name in [header::CONNECTION, header::UPGRADE] {
            if let Some(value) = inbound.get(&name) {
                headers.insert(name, value.clone());
            }
        }
        for name in [
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
        ] {
            if let Some(value) = inbound.get(name) {
                headers.insert(HeaderName::from_static(name), value.clone());
            }
        }
    }
    apply_transform(&mut headers, &resolved.effective.headers.request.transform);

    Ok(OutboundDraft {
        headers,
        body: body.to_vec(),
        query: OutboundQuery::parse(raw_query),
    })
}

/// Freeze the draft into the request that goes on the wire.
///
/// # Errors
///
/// [`ErrorKind::Validation`] when the outbound URI cannot be built.
fn finalize_outbound(
    path: &str,
    method: &Method,
    draft: OutboundDraft,
) -> Result<http::Request<Vec<u8>>, DomainError> {
    let uri = outbound_uri(path, draft.query.render())?;
    http::Request::builder()
        .method(method.clone())
        .uri(uri)
        .body(draft.body)
        .map(|mut request| {
            *request.headers_mut() = draft.headers;
            request
        })
        .map_err(|err| {
            DomainError::new(
                ErrorKind::Validation,
                format!("outbound path is not valid: {err}"),
            )
        })
}

/// Copy the inbound headers the upstream's `passthrough` rule admits.
fn passthrough_headers(resolved: &ResolvedRequest, inbound: &HeaderMap) -> HeaderMap {
    use crate::domain::model::RequestHeaderRules;
    let rules: &RequestHeaderRules = &resolved.effective.headers.request;
    let mut headers = HeaderMap::new();
    let admissible = |name: &axum::http::HeaderName| -> bool {
        // Routing headers are consumed here and never leave the gateway.
        if name.as_str() == ids::TARGET_HOST_HEADER || name == header::HOST {
            return false;
        }
        if HOP_BY_HOP.iter().any(|hop| name.as_str() == *hop) {
            return false;
        }
        match rules.passthrough {
            Passthrough::None => false,
            Passthrough::Allowlist => rules
                .passthrough_allowlist
                .iter()
                .any(|allowed| allowed.trim().eq_ignore_ascii_case(name.as_str())),
            Passthrough::All => true,
        }
    };
    for (name, value) in inbound {
        if admissible(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers
}

/// Apply one `set` / `add` / `remove` block to a header map.
///
/// A name or a value the HTTP grammar cannot express is dropped rather than
/// failing the request: the management API validates these rules at create
/// time, so reaching here means the rule was accepted when it was stored.
fn apply_transform(headers: &mut HeaderMap, transform: &crate::domain::model::HeaderTransform) {
    for name in &transform.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in &transform.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &transform.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

/// The `host[:port]` the outbound hop is addressed by.
fn authority_of(endpoint: &crate::domain::model::Endpoint) -> String {
    let standard = (endpoint.scheme == crate::domain::model::Scheme::Http && endpoint.port == 80)
        || (endpoint.scheme.is_tls() && endpoint.port == 443);
    if standard {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

/// Build the outbound request URI from its path and rendered query.
fn outbound_uri(path: &str, query: Option<String>) -> Result<http::Uri, DomainError> {
    let value = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    http::Uri::builder()
        .path_and_query(value)
        .build()
        .map_err(|err| {
            DomainError::new(
                ErrorKind::Validation,
                format!("outbound path is not valid: {err}"),
            )
        })
}

/// The `504` the gateway answers with when the upstream never answered.
fn timeout_of(resolved: &ResolvedRequest) -> DomainError {
    DomainError::new(
        ErrorKind::RequestTimeout,
        "the upstream did not produce a response within the configured timeout",
    )
    .with_alias(&resolved.upstream.alias)
    .with_upstream_id(resolved.upstream.id)
}

/// A [`DomainError`] whose detail never names a credential.
fn validation(detail: impl Into<String>) -> DomainError {
    DomainError::new(ErrorKind::Validation, detail)
}

/// The [`ProxyContext`] the plugins see.
fn context_of(
    security: &SecurityContext,
    resolved: &ResolvedRequest,
    headers: &HeaderMap,
) -> ProxyContext {
    ProxyContext {
        security: security.clone(),
        tenant_id: resolved.upstream.tenant_id,
        alias: resolved.upstream.alias.clone(),
        upstream_id: resolved.upstream.id,
        route_id: Some(resolved.route.id),
        endpoint_host: resolved.endpoint.host.clone(),
        outbound_path: resolved.outbound_path.clone(),
        client_ip: client_ip_of_headers(headers),
    }
}

/// The address a request arrived from, as `X-Forwarded-For` recorded it.
fn client_ip_of_headers(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

/// Run the request half of the plugin chain: auth, then guards, then
/// transforms (ADR 0002).
///
/// # Errors
///
/// Whatever the chain produced — an auth failure is `401`, a guard rejects
/// with `4xx`, an unresolvable binding is `503`.
async fn run_request_plugins(
    state: &ApiState,
    security: &SecurityContext,
    resolved: &ResolvedRequest,
    method: &Method,
    inbound: &HeaderMap,
    draft: &mut OutboundDraft,
) -> Result<(), DomainError> {
    if let Some(auth) = resolved.effective.auth.as_ref() {
        let plugin = state
            .service
            .plugins
            .auth_plugin(&auth.auth_type)
            .ok_or_else(|| {
                DomainError::new(
                    ErrorKind::PluginNotFound,
                    format!("auth plugin {:?} is not registered", auth.auth_type),
                )
            })?;
        plugin
            .authenticate(
                &context_of(security, resolved, inbound),
                &auth.config,
                &mut draft.headers,
                &mut draft.query,
            )
            .await?;
    }

    for binding in &resolved.effective.plugins {
        let Some(guard) = state.service.guard_plugin(binding)? else {
            continue;
        };
        guard
            .check_request(
                &context_of(security, resolved, inbound),
                config_of(binding),
                method,
                &draft.headers,
            )
            .await?;
    }

    for binding in &resolved.effective.plugins {
        let Some(transform) = state.service.transform_plugin(binding)? else {
            continue;
        };
        transform
            .transform_request(
                &context_of(security, resolved, inbound),
                config_of(binding),
                &mut draft.headers,
            )
            .await?;
    }
    Ok(())
}

/// A binding's inline configuration, or `null` when it carried none.
fn config_of(binding: &crate::domain::model::PluginBinding) -> &serde_json::Value {
    binding.config.as_ref().unwrap_or(&serde_json::Value::Null)
}

/// Turn the upstream response into the client's response, streaming the body.
#[allow(clippy::too_many_lines)]
async fn upstream_response(
    state: &ApiState,
    security: &SecurityContext,
    resolved: &ResolvedRequest,
    uri: &Uri,
    trace_id: Option<&str>,
    inbound: &HeaderMap,
    mut upstream: http::Response<hyper::body::Incoming>,
) -> Response {
    let status = upstream.status();
    if let Err(err) = check_response(state, resolved, security, status, upstream.headers()).await {
        return problem(err, uri, trace_id);
    }

    let mut headers = std::mem::take(upstream.headers_mut());
    strip_hop_by_hop(&mut headers);
    apply_transform(&mut headers, &resolved.effective.headers.response.transform);
    for binding in &resolved.effective.plugins {
        let Ok(Some(transform)) = state.service.transform_plugin(binding) else {
            continue;
        };
        if let Err(err) = transform
            .transform_response(
                &context_of(security, resolved, &headers),
                config_of(binding),
                status,
                &mut headers,
            )
            .await
        {
            return problem(err, uri, trace_id);
        }
    }
    apply_cors_headers(
        &mut headers,
        resolved.effective.cors.as_ref(),
        inbound.get(header::ORIGIN),
    );

    let body = upstream.into_body();
    let mut response = Response::builder()
        .status(status)
        .body(Body::new(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    *response.headers_mut() = headers;
    mark_upstream(&mut response);
    response
}

/// Run the response half of the plugin chain: guards validate, then transforms
/// mutate.
///
/// # Errors
///
/// Whatever the chain produced: a guard rejects with `5xx`, an unresolvable
/// binding is `503`.
async fn check_response(
    state: &ApiState,
    resolved: &ResolvedRequest,
    security: &SecurityContext,
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<(), DomainError> {
    for binding in &resolved.effective.plugins {
        let Ok(Some(guard)) = state.service.guard_plugin(binding) else {
            continue;
        };
        guard
            .check_response(
                &context_of(security, resolved, headers),
                config_of(binding),
                status,
                headers,
            )
            .await?;
    }
    Ok(())
}

/// Negotiate a protocol switch with the upstream and bridge it to the client.
///
/// The `101` head is what the client's own upgrade is waiting on, so only the
/// headers that describe the switch leave the gateway; the session that follows
/// is pumped byte for byte for as long as either side keeps it open.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn websocket_response(
    state: &ApiState,
    security: &SecurityContext,
    resolved: &ResolvedRequest,
    uri: &Uri,
    trace_id: Option<&str>,
    inbound: &HeaderMap,
    outbound: &http::Request<Vec<u8>>,
    client_upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> Response {
    // Only the head exchange is bounded; a switched session outlives the
    // request that started it.
    let budget = state.outbound.request_timeout();
    let upgraded = match tokio::time::timeout(
        budget,
        state
            .outbound
            .open_upgrade(&resolved.endpoint, outbound, budget),
    )
    .await
    {
        Ok(Ok(upgraded)) => upgraded,
        Ok(Err(err)) => return problem(err, uri, trace_id),
        Err(_) => return problem(timeout_of(resolved), uri, trace_id),
    };

    let switched = upgraded.response.status() == StatusCode::SWITCHING_PROTOCOLS;
    let status = upgraded.response.status();
    if let Err(err) =
        check_response(state, resolved, security, status, upgraded.response.headers()).await
    {
        return problem(err, uri, trace_id);
    }

    // The head is reconstructed rather than relayed: everything that is not
    // the protocol switch itself is hop-by-hop framing this hop owns.
    let mut headers = HeaderMap::new();
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    for (name, value) in upgraded.response.headers() {
        if name.as_str().starts_with("sec-websocket") {
            headers.insert(name.clone(), value.clone());
        }
    }
    apply_transform(&mut headers, &resolved.effective.headers.response.transform);
    for binding in &resolved.effective.plugins {
        let Ok(Some(transform)) = state.service.transform_plugin(binding) else {
            continue;
        };
        if let Err(err) = transform
            .transform_response(
                &context_of(security, resolved, &headers),
                config_of(binding),
                status,
                &mut headers,
            )
            .await
        {
            return problem(err, uri, trace_id);
        }
    }
    apply_cors_headers(
        &mut headers,
        resolved.effective.cors.as_ref(),
        inbound.get(header::ORIGIN),
    );

    if !switched {
        // The upstream declined: its answer is an ordinary response and the
        // transport carries nothing further of interest.
        let mut response = Response::builder()
            .status(status)
            .body(Body::from(upgraded.response.into_body()))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
        *response.headers_mut() = headers;
        mark_upstream(&mut response);
        return response;
    }

    let mut response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    *response.headers_mut() = headers;
    mark_upstream(&mut response);

    // The client's half of the switch can only resolve once hyper has flushed
    // the `101`, which happens after this handler returns, so it is awaited on
    // the pump's own task rather than here.
    tokio::spawn(async move {
        let client_io = match client_upgrade {
            Some(upgrade) => match upgrade.await {
                Ok(io) => io,
                Err(err) => {
                    tracing::debug!(error = %err, "oagw client upgrade failed");
                    return;
                }
            },
            // Unreachable: `websocket_response` is only entered for upgrades. A
            // missing handle has nothing to bridge, so the session just ends.
            None => {
                tracing::debug!("oagw client requested no upgrade to bridge");
                return;
            }
        };
        // `Upgraded` speaks hyper's IO traits; `TokioIo` lifts it onto tokio's.
        // The upstream side carries whatever the head reader had already read
        // past the `101` — part of the session, so it is replayed first.
        let mut upstream_io =
            crate::infra::outbound::Bridge::new(upgraded.response.into_body(), upgraded.io);
        let mut client_io = hyper_util::rt::TokioIo::new(client_io);
        if let Err(err) = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
            tracing::debug!(error = %err, "oagw websocket session closed");
        }
    });
    response
}

/// The CORS check an actual cross-origin request is subject to (ADR 0004).
///
/// # Errors
///
/// A `403` when the origin or the method is not allowed. CORS is inactive
/// unless the upstream enabled it, in which case no check happens at all.
fn check_cors(
    resolved: &ResolvedRequest,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), DomainError> {
    let Some(cors) = resolved.effective.cors.as_ref().filter(|cors| cors.enabled) else {
        return Ok(());
    };
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };
    if !origin_allowed(cors, origin) {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("origin {origin:?} is not in the upstream's allowed_origins"),
        )
        .with_status(403));
    }
    if !cors
        .allowed_methods
        .iter()
        .any(|allowed| allowed.trim().eq_ignore_ascii_case(method.as_str()))
    {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("method {method} is not in the upstream's allowed_methods"),
        )
        .with_status(403));
    }
    Ok(())
}

/// Exact, protocol- and port-sensitive origin matching (ADR 0004).
fn origin_allowed(cors: &CorsRule, origin: &str) -> bool {
    cors.allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// The CORS headers an actual cross-origin request carries back.
fn apply_cors_headers(
    headers: &mut HeaderMap,
    cors: Option<&CorsRule>,
    origin: Option<&HeaderValue>,
) {
    let Some(cors) = cors.filter(|cors| cors.enabled) else {
        return;
    };
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        append_vary(headers, "Origin");
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", ")) {
            headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
        }
    if cors.allow_credentials {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
}

/// Add `name` to `Vary`, keeping any values already there.
fn append_vary(headers: &mut HeaderMap, name: &str) {
    let existing = headers
        .get(header::VARY)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let value = match existing {
        Some(existing)
            if existing
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(name)) =>
        {
            return;
        }
        Some(existing) => format!("{existing}, {name}"),
        None => name.to_owned(),
    };
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(header::VARY, value);
    }
}

/// Strip the hop-by-hop headers from a header map.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    // Framing is recomputed for the client hop.
    headers.remove(header::CONTENT_LENGTH);
}

/// Mark a response as having come from the upstream (ADR 0007).
fn mark_upstream(response: &mut Response) {
    response.headers_mut().insert(
        HeaderName::from_static(ids::ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ids::ERROR_SOURCE_UPSTREAM),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(scheme: crate::domain::model::Scheme, port: u16) -> crate::domain::model::Endpoint {
        crate::domain::model::Endpoint {
            scheme,
            host: "api.example.com".to_owned(),
            port,
        }
    }

    #[test]
    fn the_proxy_path_splits_into_alias_and_suffix() {
        assert_eq!(
            split_proxy_path("/oagw/v1/proxy/api.openai.com"),
            Some(("api.openai.com".to_owned(), None))
        );
        assert_eq!(
            split_proxy_path("/oagw/v1/proxy/api.openai.com/v1/chat/completions"),
            Some((
                "api.openai.com".to_owned(),
                Some("v1/chat/completions".to_owned())
            ))
        );
        assert_eq!(split_proxy_path("/oagw/v1/proxy/"), None);
        assert_eq!(split_proxy_path("/api/oagw/v1/proxy/alias"), None);
    }

    #[test]
    fn a_percent_escaped_suffix_survives_untouched() {
        let (alias, suffix) = split_proxy_path("/oagw/v1/proxy/svc/a%2Fb?x=1").expect("split");
        assert_eq!(alias, "svc");
        assert_eq!(suffix.as_deref(), Some("a%2Fb"));
    }

    #[test]
    fn a_preflight_is_options_with_the_cors_headers() {
        let mut headers = HeaderMap::new();
        assert!(!is_preflight(&Method::OPTIONS, &headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://app.example.com"),
        );
        assert!(!is_preflight(&Method::OPTIONS, &headers));
        headers.insert(
            "access-control-request-method",
            HeaderValue::from_static("POST"),
        );
        assert!(is_preflight(&Method::OPTIONS, &headers));
        assert!(!is_preflight(&Method::GET, &headers));
    }

    #[test]
    fn only_a_websocket_upgrade_is_detected_as_one() {
        let mut headers = HeaderMap::new();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!is_websocket_upgrade(&headers));
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        strip_hop_by_hop(&mut headers);
        assert!(headers.get(header::CONNECTION).is_none());
        assert!(headers.get(header::TRANSFER_ENCODING).is_none());
        assert_eq!(
            headers.get(header::CONTENT_TYPE).map(|v| v.as_bytes()),
            Some(b"application/json".as_slice())
        );
    }

    #[test]
    fn vary_accumulates_without_duplicates() {
        let mut headers = HeaderMap::new();
        append_vary(&mut headers, "Origin");
        append_vary(&mut headers, "origin");
        assert_eq!(
            headers.get(header::VARY).map(|v| v.as_bytes()),
            Some(b"Origin".as_slice())
        );
        append_vary(&mut headers, "Accept-Encoding");
        assert_eq!(
            headers.get(header::VARY).map(|v| v.as_bytes()),
            Some(b"Origin, Accept-Encoding".as_slice())
        );
    }

    #[test]
    fn standard_ports_are_omitted_from_the_authority() {
        assert_eq!(
            authority_of(&endpoint(crate::domain::model::Scheme::Https, 443)),
            "api.example.com"
        );
        assert_eq!(
            authority_of(&endpoint(crate::domain::model::Scheme::Http, 80)),
            "api.example.com"
        );
        assert_eq!(
            authority_of(&endpoint(crate::domain::model::Scheme::Http, 8080)),
            "api.example.com:8080"
        );
    }

    #[test]
    fn an_outbound_uri_carries_the_query() {
        assert_eq!(
            outbound_uri("/v1/chat", Some("model=gpt-4".to_owned())).expect("uri"),
            "/v1/chat?model=gpt-4"
        );
        assert_eq!(outbound_uri("/v1/chat", None).expect("uri"), "/v1/chat");
    }

    #[test]
    fn a_wildcard_origin_admits_every_requesting_origin() {
        let mut cors = CorsRule {
            enabled: true,
            ..CorsRule::default()
        };
        cors.allowed_origins = vec!["*".to_owned()];
        assert!(origin_allowed(&cors, "https://anywhere.example.org"));
        cors.allowed_origins = vec!["https://app.example.com".to_owned()];
        assert!(origin_allowed(&cors, "https://app.example.com"));
        // Port and protocol sensitive.
        assert!(!origin_allowed(&cors, "https://app.example.com:8443"));
        assert!(!origin_allowed(&cors, "http://app.example.com"));
        assert!(!origin_allowed(&cors, "https://evil.example.com"));
    }

    #[test]
    fn a_cors_rejection_is_403() {
        let cors = CorsRule {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            ..CorsRule::default()
        };
        let resolved = crate::domain::service::ResolvedRequest {
            upstream: crate::domain::model::Upstream {
                id: uuid::Uuid::new_v4(),
                tenant_id: uuid::Uuid::new_v4(),
                enabled: true,
                alias: "api.example.com".to_owned(),
                tags: Vec::new(),
                server: crate::domain::model::ServerConfig::default(),
                protocol: crate::domain::model::Protocol::Http,
                auth: None,
                headers: crate::domain::model::HeaderRules::default(),
                plugins: crate::domain::model::PluginSet::default(),
                rate_limit: None,
                cors: Some(cors.clone()),
                created_at: None,
                updated_at: None,
            },
            route: test_route(),
            endpoint: endpoint(crate::domain::model::Scheme::Https, 443),
            chain: vec![uuid::Uuid::new_v4()],
            effective: crate::domain::service::EffectiveConfig {
                cors: Some(cors),
                ..Default::default()
            },
            outbound_path: "/v1".to_owned(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example.com"),
        );
        let err = check_cors(&resolved, &Method::GET, &headers).expect_err("rejected");
        assert_eq!(err.status(), 403);
    }

    fn test_route() -> crate::domain::model::Route {
        crate::domain::model::Route {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            enabled: true,
            tags: Vec::new(),
            upstream_id: uuid::Uuid::new_v4(),
            match_rule: crate::domain::model::MatchRule::Http(crate::domain::model::HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            plugins: crate::domain::model::PluginSet::default(),
            rate_limit: None,
            created_at: None,
            updated_at: None,
        }
    }
}
