//! The proxy handler.
//!
//! One implementation serves `{METHOD} /proxy/{alias}[/{path}]` for every
//! method axum can route; the router calls into it with the alias and path
//! suffix already extracted, so the alias never collides with route matching.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt as _;
use toolkit_security::SecurityContext;

use crate::api::rest::error::problem_from_domain_error;
use crate::api::rest::extractors::{take_target_host, validate_target_host};
use crate::api::rest::state::OagwState;
use crate::domain::cors::{self, PreflightOutcome, VARY_HEADER};
use crate::domain::error::DomainError;
use crate::domain::matching;
use crate::infra::proxy::connector::UpstreamRequest;
use crate::infra::proxy::websocket;

/// Hop-by-hop headers never forwarded in either direction (RFC 9110 §7.6.1).
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

/// The streaming content type the gateway forwards chunk by chunk.
const EVENT_STREAM: &str = "text/event-stream";

/// Serves `{METHOD} /proxy/{alias}`.
///
/// The security context is optional: a browser preflight arrives with no
/// credentials at all (ADR 0004 §"Preflight Request Handling"), so the handler
/// must be reachable — and answer 204 — without one. An actual request that
/// carries no context is still refused by the authorization layer.
pub async fn proxy(
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    ctx: Option<axum::Extension<SecurityContext>>,
    axum::extract::Path(alias): axum::extract::Path<String>,
    request: axum::extract::Request,
) -> Response {
    serve(state, ctx.map(|e| e.0), alias, String::new(), request).await
}

/// Serves `{METHOD} /proxy/{alias}/{path}`.
pub async fn proxy_with_path(
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    ctx: Option<axum::Extension<SecurityContext>>,
    axum::extract::Path((alias, path)): axum::extract::Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    serve(state, ctx.map(|e| e.0), alias, path, request).await
}

/// The proxy entry point with alias and path suffix already split out.
async fn serve(
    state: Arc<OagwState>,
    ctx: Option<SecurityContext>,
    alias: String,
    path: String,
    request: axum::extract::Request,
) -> Response {
    let instance = format!("/oagw/v1/proxy/{alias}");
    match handle(&state, ctx.as_ref(), &alias, &path, request).await {
        Ok(response) => response,
        Err(failure) => {
            let mut problem = problem_from_domain_error(&failure.error, &instance).into_response();
            if let Some(plan) = &failure.plan {
                // The error half of the transform chain still runs: PRD
                // "Transform(response/error)".
                let mut error_ctx = crate::domain::plugin::ErrorContext {
                    error: Some(failure.error.clone()),
                    source: Some("gateway".to_string()),
                    attributes: std::collections::BTreeMap::new(),
                    headers: std::collections::BTreeMap::new(),
                };
                state
                    .data_plane
                    .run_error_plugins(plan, &mut error_ctx, &failure.attributes)
                    .await;
                let headers = problem.headers_mut();
                for (name, values) in error_ctx.headers {
                    for value in values {
                        if let (Ok(parsed), Ok(rendered)) = (
                            HeaderName::from_bytes(name.as_bytes()),
                            HeaderValue::from_str(&value),
                        ) {
                            headers.append(parsed, rendered);
                        }
                    }
                }
            }
            problem
        }
    }
}

/// A failure the proxy renders, carrying what the error half of the plugin
/// chain needs: the plan the request had reached and the attributes the
/// request-phase transforms recorded.
#[derive(Debug)]
struct ProxyFailure {
    error: DomainError,
    plan: Option<Arc<crate::infra::proxy::service::ProxyPlan>>,
    attributes: std::collections::BTreeMap<String, String>,
}

impl From<DomainError> for ProxyFailure {
    fn from(error: DomainError) -> Self {
        ProxyFailure::unplaned(error)
    }
}

impl ProxyFailure {
    /// A failure raised before the request had a plan.
    fn unplaned(error: DomainError) -> Self {
        Self {
            error,
            plan: None,
            attributes: std::collections::BTreeMap::new(),
        }
    }

    /// A failure raised once the plan existed.
    fn planned(
        error: DomainError,
        plan: &Arc<crate::infra::proxy::service::ProxyPlan>,
        attributes: std::collections::BTreeMap<String, String>,
    ) -> Self {
        Self {
            error,
            plan: Some(plan.clone()),
            attributes,
        }
    }
}

async fn handle(
    state: &Arc<OagwState>,
    ctx: Option<&SecurityContext>,
    alias: &str,
    path: &str,
    request: axum::extract::Request,
) -> Result<Response, ProxyFailure> {
    let data_plane = &state.data_plane;

    let (method, raw_query, query, mut headers, request) = split_request(request);
    let origin = header_str(&headers, "origin");
    let request_method = header_str(&headers, "access-control-request-method");

    // A preflight is answered permissively from the request alone (ADR 0004):
    // no tenant resolution, no upstream resolution and no plugin run, before
    // anything else. A browser sends no credentials on a preflight, so the
    // tenant chain cannot be resolved for one — testing it first would turn
    // every credential-less preflight into a 403/500. Origin and method
    // enforcement is deferred to the actual request that follows.
    if cors::is_preflight(&method, origin.as_deref(), request_method.as_deref()) {
        return Ok(preflight_response(PreflightOutcome::permissive(
            origin.as_deref(),
            request_method.as_deref(),
            header_str(&headers, "access-control-request-headers").as_deref(),
        )));
    }

    // An actual request must name a caller. One that arrives without a
    // security context resolves to the nil tenant, which the authorization
    // layer refuses, so the proxy never forwards an unattributed request.
    let ctx = ctx.cloned().unwrap_or_else(anonymous_context);
    let chain = state.tenant_chain(&ctx).await.map_err(ProxyFailure::unplaned)?;

    // The router hands the path suffix over without its leading slash; route
    // matching and the forwarded target both speak upstream paths, so it is
    // restored here (`/proxy/openai/v1/chat/completions` → `/v1/chat/completions`).
    let path = if path.is_empty() { String::new() } else { format!("/{path}") };

    let Some(resolved) = data_plane.resolve(&chain, alias).await.map_err(ProxyFailure::unplaned)? else {
        // The PRD calls an unresolvable alias a 404 RouteNotFound, the same
        // row an unmatched route produces.
        return Err(ProxyFailure::unplaned(DomainError::RouteNotFound {
            detail: format!("no upstream is registered under alias `{alias}`"),
            alias: Some(alias.to_string()),
            host: None,
            path: Some(path.to_string()),
        }));
    };

    // Route matching: method allowlist, longest path prefix, query allowlist.
    let candidates = state
        .control_plane
        .proxy_routes(&chain)
        .await
        .map_err(ProxyFailure::unplaned)?;
    let candidates: Vec<crate::domain::dto::Route> =
        candidates.into_iter().map(|(_, route)| route).collect();
    let matched = matching::best_route(&candidates, &method, &path, &query)
        .ok_or_else(|| {
            ProxyFailure::unplaned(DomainError::RouteNotFound {
                detail: format!("no route on upstream `{alias}` matches {method} /{path}"),
                alias: Some(alias.to_string()),
                host: None,
                path: Some(path.to_string()),
            })
        })?;
    let plan = Arc::new(
        data_plane
            .plan(&chain, &resolved, Some(&matched), &method, &path, &query)
            .await
            .map_err(ProxyFailure::unplaned)?,
    );

    // An explicit target-host hint narrows the routing without re-resolving.
    // Consuming it here also removes it from the headers that are forwarded.
    if let Some(hint) = take_target_host(&mut headers) {
        let validated = validate_target_host(&hint).map_err(|error| {
            ProxyFailure::planned(error, &plan, std::collections::BTreeMap::new())
        })?;
        if validated != plan.endpoint_host {
            return Err(ProxyFailure::planned(
                DomainError::UnknownTargetHost {
                    invalid_value: validated,
                    valid_hosts: vec![plan.endpoint_host.clone()],
                },
                &plan,
                std::collections::BTreeMap::new(),
            ));
        }
    }

    // CORS is enforced on actual cross-origin requests, after upstream
    // resolution and before anything is forwarded (ADR 0004).
    let instance = format!("/oagw/v1/proxy/{alias}");
    if let Err(rejection) = cors::enforce(data_plane.cors_of(&plan), origin.as_deref(), &method) {
        let invalid_value = rejection.invalid_value().to_string();
        return Ok(match rejection {
            cors::CorsRejection::Origin(_) => crate::api::rest::error::cors_origin_not_allowed(
                &invalid_value,
                &instance,
            )
            .into_response(),
            cors::CorsRejection::Method(_) => crate::api::rest::error::cors_method_not_allowed(
                &invalid_value,
                &instance,
            )
            .into_response(),
        });
    }

    // Plugins run before the rate limit: a rejected request consumes no token.
    let mut plugin_ctx = build_request_context(&plan, &method, &path, &raw_query, &headers, &ctx);
    data_plane
        .run_request_plugins(&plan, &mut plugin_ctx)
        .await
        .map_err(ProxyFailure::unplaned)?;
    let mut attributes = BTreeMap::new();
    if let Some(id) = plugin_ctx.request_id.clone() {
        attributes.insert("oagw.request_id".to_string(), id);
    }
    let snapshot = data_plane
        .enforce_rate_limit(&plan, &plugin_ctx)
        .await
        .map_err(|error| ProxyFailure::planned(error, &plan, attributes.clone()))?;

    // A WebSocket upgrade is a transport change, not a proxied body.
    if websocket::is_websocket_upgrade(
        &method,
        header_str(&headers, "upgrade").as_deref(),
        header_str(&headers, "connection").as_deref(),
    ) {
        return upgrade(state, &plan, &headers, request)
            .await
            .map_err(|error| ProxyFailure::planned(error, &plan, attributes.clone()));
    }

    let body = read_body(&headers, request, state.config.max_body_bytes)
        .await
        .map_err(|error| ProxyFailure::planned(error, &plan, attributes.clone()))?;
    let mut plugin_ctx = plugin_ctx;
    plugin_ctx.body = body.to_vec();

    let outgoing = UpstreamRequest {
        method: method.clone(),
        path: plan.forward_path.clone(),
        query: plan.forward_query.clone().or_else(|| {
            if raw_query.is_empty() {
                None
            } else {
                Some(raw_query.clone())
            }
        }),
        headers: outbound_headers(&plan, &plugin_ctx, &plan.endpoint_host),
        body: Some(body),
    };
    let mut exchange = data_plane
        .send(&plan, outgoing)
        .await
        .map_err(|error| ProxyFailure::planned(error, &plan, attributes.clone()))?;
    let status = exchange.status();
    let response_headers = exchange.headers();

    if is_streaming(&response_headers) {
        let mut builder =
            Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY));
        apply_headers(&mut builder, &plan, &response_headers, &snapshot, &attributes, origin.as_deref());
        let headers = builder.headers_mut().expect("response builder");
        for name in HOP_BY_HOP {
            if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
                headers.remove(parsed);
            }
        }
        return Ok(builder
            .body(Body::from_stream(exchange.into_body_stream()))
            .expect("valid response"));
    }

    let body = exchange.body_bytes(state.config.max_body_bytes).await?;
    let mut response_ctx = crate::domain::plugin::ResponseContext::default();
    response_ctx.status = Some(status);
    response_ctx.headers = group_headers(&response_headers);
    response_ctx.body = body.to_vec();
    data_plane
        .run_response_plugins(&plan, &mut response_ctx, &attributes)
        .await
        .map_err(|error| ProxyFailure::planned(error, &plan, attributes.clone()))?;

    let mut builder =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY));
    let flattened: Vec<(String, String)> = response_ctx
        .headers
        .iter()
        .flat_map(|(name, values)| values.iter().map(|v| (name.clone(), v.clone())))
        .collect();
    apply_headers(&mut builder, &plan, &flattened, &snapshot, &attributes, origin.as_deref());
    let headers = builder.headers_mut().expect("response builder");
    for name in HOP_BY_HOP {
        if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(parsed);
        }
    }
    Ok(builder.body(Body::from(response_ctx.body)).expect("valid response"))
}

/// Splits an incoming request into the parts the proxy needs.
fn split_request(
    request: axum::extract::Request,
) -> (
    String,
    String,
    Vec<(String, String)>,
    HeaderMap,
    axum::extract::Request,
) {
    let method = request.method().as_str().to_string();
    let raw_query = request.uri().query().unwrap_or_default().to_string();
    let query = crate::api::rest::extractors::parse_query(&raw_query);
    let mut request = request;
    let headers = std::mem::take(request.headers_mut());
    (method, raw_query, query, headers, request)
}

/// Proxies a WebSocket upgrade.
async fn upgrade(
    state: &Arc<OagwState>,
    plan: &crate::infra::proxy::service::ProxyPlan,
    headers: &HeaderMap,
    request: axum::extract::Request,
) -> Result<Response, DomainError> {
    let client_key = header_str(headers, "sec-websocket-key").ok_or_else(|| {
        DomainError::ProtocolError {
            detail: "the WebSocket handshake carries no Sec-WebSocket-Key".to_string(),
            host: Some(plan.endpoint_host.clone()),
        }
    })?;
    let connector = state.data_plane.connector();
    let peer = connector.peer(
        &plan.endpoint_host,
        plan.endpoint_port,
        plan.endpoint_tls,
    )?;
    let mut upstream = connector.open_raw(&peer).await?;
    let upstream_key = websocket::new_client_key();
    let target = match plan.forward_query.as_deref() {
        Some(query) => format!("{}?{}", plan.forward_path, query),
        None => plan.forward_path.clone(),
    };
    let mut handshake = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n",
        target = target,
        host = plan.endpoint_host,
        key = upstream_key,
    );
    for (name, value) in headers.iter() {
        let name = name.as_str().to_ascii_lowercase();
        // The key and version are re-originated above, so the caller's copies
        // are not forwarded: a handshake carrying two of either is ambiguous.
        if name == "host"
            || name == "sec-websocket-key"
            || name == "sec-websocket-version"
            || HOP_BY_HOP.contains(&name.as_str())
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            handshake.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    handshake.push_str("\r\n");

    use tokio::io::AsyncWriteExt;
    upstream
        .write_all(handshake.as_bytes())
        .await
        .map_err(|err| DomainError::LinkUnavailable {
            detail: format!("the WebSocket handshake could not be sent: {err}"),
            upstream_id: Some(plan.config.upstream_id.clone()),
            alias: Some(plan.config.alias.clone()),
        })?;
    let reply = read_handshake_reply(&mut upstream).await?;
    if reply.status != 101 {
        return Err(DomainError::DownstreamError {
            status: reply.status,
            host: Some(plan.endpoint_host.clone()),
        });
    }
    let expected = websocket::accept_key(&upstream_key);
    if !reply.accept.as_deref().is_some_and(|v| v == expected) {
        return Err(DomainError::ProtocolError {
            detail: "the upstream WebSocket handshake did not echo the negotiated key".to_string(),
            host: Some(plan.endpoint_host.clone()),
        });
    }

    // The upgraded connection only exists once hyper has written the `101`, so
    // the reply is returned first and the relay picks the connection up from
    // the spawned task; awaiting it here would deadlock the handshake.
    let downstream = hyper::upgrade::on(request);
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    if let Ok(value) = HeaderValue::from_str(&websocket::accept_key(&client_key)) {
        builder = builder.header("sec-websocket-accept", value);
    }
    if let Some(protocol) = reply.protocol.as_deref() {
        if let Ok(value) = HeaderValue::from_str(protocol) {
            builder = builder.header("sec-websocket-protocol", value);
        }
    }
    let response = builder.body(Body::empty()).expect("valid response");

    tokio::spawn(async move {
        let Ok(downstream) = downstream.await else {
            return; // the client gave up before the upgrade was taken
        };
        let mut io = hyper_util::rt::TokioIo::new(downstream);
        let _ = tokio::io::copy_bidirectional(&mut upstream, &mut io).await;
    });
    Ok(response)
}

/// Reads the upstream's `101` reply off a raw stream.
async fn read_handshake_reply(
    stream: &mut pingora_core::protocols::Stream,
) -> Result<HandshakeReply, DomainError> {
    use tokio::io::AsyncReadExt;
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|_| downstream_refused())?;
        if read == 0 {
            return Err(downstream_refused());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let (head, _) = buffer.split_at(end + 4);
            let mut parsed = [httparse::EMPTY_HEADER; 32];
            let mut response = httparse::Response::new(&mut parsed);
            let parsed = response.parse(head).map_err(|_| DomainError::ProtocolError {
                detail: "the upstream WebSocket handshake was malformed".to_string(),
                host: None,
            })?;
            if matches!(parsed, httparse::Status::Partial) {
                return Err(DomainError::ProtocolError {
                    detail: "the upstream WebSocket handshake was incomplete".to_string(),
                    host: None,
                });
            }
            return Ok(HandshakeReply {
                status: response.code.unwrap_or(502),
                accept: header_value(response.headers, "sec-websocket-accept"),
                protocol: header_value(response.headers, "sec-websocket-protocol"),
            });
        }
        if buffer.len() > 64 * 1024 {
            return Err(downstream_refused());
        }
    }
}

/// A generic 502 for an upstream that would not speak to us.
fn downstream_refused() -> DomainError {
    DomainError::DownstreamError { status: 502, host: None }
}

/// The parsed reply to an upgrade handshake.
struct HandshakeReply {
    status: u16,
    accept: Option<String>,
    protocol: Option<String>,
}

/// The first value of a parsed header.
fn header_value(headers: &[httparse::Header<'_>], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| String::from_utf8(h.value.to_vec()).ok())
}

/// Renders a preflight outcome as a response.
fn preflight_response(outcome: PreflightOutcome) -> Response {
    if !outcome.allowed {
        let mut problem = problem_from_domain_error(
            &DomainError::PermissionDenied {
                detail: "the origin is not allowed by the upstream's CORS policy".to_string(),
            },
            "/oagw/v1/proxy",
        );
        problem.status = StatusCode::FORBIDDEN.as_u16();
        return problem.into_response();
    }
    let mut builder = Response::builder().status(StatusCode::NO_CONTENT);
    if let Some(origin) = outcome.allow_origin.as_deref() {
        builder = builder.header("access-control-allow-origin", origin);
    }
    if let Some(methods) = outcome.allow_methods.as_deref() {
        builder = builder.header("access-control-allow-methods", methods);
    }
    if let Some(allow) = outcome.allow_headers.as_deref() {
        builder = builder.header("access-control-allow-headers", allow);
    }
    if outcome.allow_credentials {
        builder = builder.header("access-control-allow-credentials", "true");
    }
    if let Some(expose) = outcome.expose_headers.as_deref() {
        builder = builder.header("access-control-expose-headers", expose);
    }
    builder
        .header("access-control-max-age", outcome.max_age.to_string())
        .header("vary", VARY_HEADER)
        .header("X-OAGW-Error-Source", "gateway")
        .body(Body::empty())
        .expect("static response")
}

/// Reads the request body, enforcing DESIGN §"Body Validation Rules": a
/// `Content-Length` that parses and matches the body, a `Transfer-Encoding`
/// the gateway supports, and the configured hard cap — applied before the
/// body is buffered.
async fn read_body(
    headers: &HeaderMap,
    request: axum::extract::Request,
    limit: usize,
) -> Result<Bytes, DomainError> {
    if let Some(encoding) = header_str(headers, "transfer-encoding").as_deref() {
        let unsupported: Vec<String> = encoding
            .split(',')
            .map(|token| token.trim().to_ascii_lowercase())
            .filter(|token| token != "chunked")
            .collect();
        if !unsupported.is_empty() {
            return Err(DomainError::ValidationError {
                detail: format!(
                    "`transfer-encoding` `{encoding}` is not supported, only `chunked`"
                ),
            });
        }
    }

    let declared = match header_str(headers, "content-length").as_deref() {
        Some(value) => Some(value.trim().parse::<usize>().map_err(|_| {
            DomainError::ValidationError {
                detail: format!("`content-length` `{value}` is not a valid integer"),
            }
        })?),
        None => None,
    };
    if declared.is_some_and(|length| length > limit) {
        return Err(too_large(limit));
    }

    let mut buffered: Vec<u8> = Vec::new();
    let mut stream = request.into_body().into_data_stream();
    while let Some(frame) = stream.next().await {
        let frame = frame.map_err(|err| DomainError::ProtocolError {
            detail: format!("the request body could not be read: {err}"),
            host: None,
        })?;
        if buffered.len() + frame.len() > limit {
            // The cap is applied as soon as the next frame would overflow it,
            // so nothing past the limit is ever buffered (DESIGN §"Body
            // Validation Rules": "reject before buffering").
            return Err(too_large(limit));
        }
        buffered.extend_from_slice(&frame);
    }

    if let Some(length) = declared {
        if length != buffered.len() {
            return Err(DomainError::ValidationError {
                detail: format!(
                    "`content-length` `{length}` does not match the {len} byte body",
                    len = buffered.len()
                ),
            });
        }
    }
    Ok(Bytes::from(buffered))
}

/// The 413 problem for a body that exceeds the configured cap.
fn too_large(limit: usize) -> DomainError {
    DomainError::PayloadTooLarge {
        detail: format!("the request body exceeds the {limit} byte limit"),
    }
}

/// The context of a request that arrived with none: the nil tenant, which the
/// authorization layer refuses. It exists so a credential-less preflight can
/// be answered (ADR 0004) without manufacturing a caller for it.
fn anonymous_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::nil())
        .subject_tenant_id(uuid::Uuid::nil())
        .build()
        .expect("a valid anonymous context")
}

/// Builds the mutable plugin context for a request.
fn build_request_context(
    plan: &crate::infra::proxy::service::ProxyPlan,
    method: &str,
    path: &str,
    raw_query: &str,
    headers: &HeaderMap,
    ctx: &SecurityContext,
) -> crate::domain::plugin::RequestContext {
    let mut plugin_ctx = crate::domain::plugin::RequestContext {
        upstream_id: Some(plan.config.upstream_id.clone()),
        alias: Some(plan.config.alias.clone()),
        path: Some(plan.forward_path.clone()),
        query: Some(raw_query.to_string()),
        method: Some(method.to_string()),
        tenant_id: Some(plan.owner_tenant_id.to_string()),
        principal_id: Some(ctx.subject_id().to_string()),
        client_ip: None,
        ..Default::default()
    };
    for (name, value) in headers.iter() {
        if let Ok(value) = value.to_str() {
            plugin_ctx
                .headers
                .entry(name.as_str().to_ascii_lowercase())
                .or_default()
                .push(value.to_string());
        }
    }
    plugin_ctx.path = Some(path.to_string());
    plugin_ctx
}

/// Builds the outbound header list: transformed, then stripped of hop-by-hop.
fn outbound_headers(
    plan: &crate::infra::proxy::service::ProxyPlan,
    plugin_ctx: &crate::domain::plugin::RequestContext,
    endpoint_host: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, values) in &plugin_ctx.headers {
        for value in values {
            out.push((name.clone(), value.clone()));
        }
    }
    if let Some(rules) = plan.config.headers.as_ref() {
        let request_rules = rules.request.as_ref();
        if let Some(rules) = request_rules {
            for (name, value) in &rules.set {
                out.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
                out.push((name.clone(), value.clone()));
            }
            for (name, value) in &rules.add {
                out.push((name.clone(), value.clone()));
            }
            for name in &rules.remove {
                out.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
            }
        }
    }
    out.retain(|(n, _)| {
        !n.eq_ignore_ascii_case("host")
            && !n.eq_ignore_ascii_case("content-length")
            && !HOP_BY_HOP.contains(&n.as_str())
    });
    out.push(("host".to_string(), endpoint_host.to_ascii_lowercase()));
    out
}

/// The effective CORS policy of a plan, if it configured one.
fn data_plane_cors(plan: &crate::infra::proxy::service::ProxyPlan) -> Option<&crate::domain::dto::Cors> {
    plan.config.cors.as_ref()
}

/// Applies an upstream's response headers to a builder.
fn apply_headers(
    builder: &mut axum::http::response::Builder,
    plan: &crate::infra::proxy::service::ProxyPlan,
    headers: &[(String, String)],
    snapshot: &crate::domain::error::RateLimitSnapshot,
    attributes: &BTreeMap<String, String>,
    origin: Option<&str>,
) {
    let sink = builder.headers_mut().expect("response builder");
    for (name, value) in headers {
        if HOP_BY_HOP.contains(&name.as_str()) || name == "content-length" {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            sink.insert(name, value);
        }
    }
    if let Some(rules) = plan.config.headers.as_ref() {
        let response_rules = rules.response.as_ref();
        if let Some(rules) = response_rules {
        for (name, value) in &rules.set {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                sink.insert(name, value);
            }
        }
        for name in &rules.remove {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                sink.remove(name);
            }
        }
        }
    }
    // ADR 0003: `rate_limit.response_headers` (default true) decides whether
    // the proxy advertises the counters; a merged layer that withholds them
    // wins over one that publishes them.
    let advertise = plan
        .config
        .rate_limit
        .as_ref()
        .map(|r| r.response_headers)
        .unwrap_or(true);
    if advertise && snapshot.limit > 0 {
        for (name, value) in [
            ("x-ratelimit-limit", snapshot.limit.to_string()),
            ("x-ratelimit-remaining", snapshot.remaining.to_string()),
            ("x-ratelimit-reset", snapshot.reset.to_string()),
        ] {
            if let Ok(value) = HeaderValue::from_str(&value) {
                sink.insert(name, value);
            }
        }
    }
    if let Some(request_id) = attributes.get("oagw.request_id") {
        if let Ok(value) = HeaderValue::from_str(request_id) {
            sink.insert("x-request-id", value);
        }
    }
    // ADR 0004: a proxied response to a cross-origin request carries the
    // policy's CORS headers, and always a `Vary` that names `Origin` so no
    // cache can serve one origin's response to another.
    if let Some(origin) = origin {
        for (name, value) in cors::response_headers(data_plane_cors(plan), origin) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                sink.insert(name, value);
            }
        }
        sink.insert("vary", HeaderValue::from_static("Origin"));
    }
    sink.insert(
        "x-oagw-error-source",
        HeaderValue::from_static("upstream"),
    );
}

/// Whether a response should stream rather than buffer.
fn is_streaming(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type") && value.starts_with(EVENT_STREAM)
    })
}

/// The first value of a header.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|s| s.to_string())
}

/// Groups response header pairs by name, preserving values in order.
fn group_headers(headers: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        out.entry(name.clone()).or_default().push(value.clone());
    }
    out
}
