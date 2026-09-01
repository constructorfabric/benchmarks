//! REST handlers of the management and proxy APIs.
//!
//! Handlers stay transport-thin: they bind the request, map the authenticated
//! caller onto a [`crate::domain::dto::RequestContext`], call the control
//! plane, and shape the response. Errors cross the boundary through
//! [`OagwProblem`], which carries the GTS `type` and the `X-OAGW-Error-Source`
//! header of `DESIGN` §3.3; success responses carry the same header through
//! [`with_error_source`].

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, RawQuery};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use toolkit::api::{apply_select, extract_trace_id};
use toolkit_security::SecurityContext;

use super::dto::{
    ListEnvelopeDto, PageMetaDto, PluginRequestDto, PluginResponseDto, PluginSourceResponseDto,
    RouteRequestDto, RouteResponseDto, UpstreamRequestDto, UpstreamResponseDto, build_list_query,
    parse_plugin_id, parse_route_id, parse_upstream_id,
};
use super::error::{ApiResult, OagwProblem, with_error_source};
use crate::domain::dto::ListQuery;
use crate::domain::error::{DomainError, OagwErrorType};
use crate::domain::model::canonical_types;
use crate::domain::services::ControlPlaneService;
use crate::infra::proxy::forward::{ForwardOutcome, ForwardRequest, ProxyEngine};

/// The handler-visible control plane.
pub type ControlPlane = Arc<ControlPlaneService>;

/// Build a tenant-scoped request context from the authenticated caller.
///
/// The correlation id is resolved here, once per request, from the platform
/// headers (`traceparent`, `x-trace-id`, `x-request-id`), so every problem this
/// handler emits can carry it.
///
/// # Errors
/// Returns [`OagwProblem`] with `403` when the caller carries no tenant
/// identity.
pub fn request_context(
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    security: &SecurityContext,
) -> Result<crate::domain::dto::RequestContext, OagwProblem> {
    let tenant = security.subject_tenant_id();
    if tenant.is_nil() {
        return Err(stamped(
            uri,
            headers,
            OagwProblem::canonical(
                canonical_types::PERMISSION_DENIED,
                "Forbidden",
                403,
                "the caller carries no tenant identity",
            ),
        ));
    }
    Ok(crate::domain::dto::RequestContext {
        tenant,
        subject: security.subject_id().to_string(),
    })
}

/// Stamp the request path and the correlation id onto a problem.
fn stamped(uri: &axum::http::Uri, headers: &HeaderMap, problem: OagwProblem) -> OagwProblem {
    let problem = problem.with_instance(uri.path().to_owned());
    match extract_trace_id(headers) {
        Some(trace_id) => problem.with_trace_id(trace_id),
        None => problem,
    }
}

/// Build the `$select` projection of a page.
fn envelope<T>(
    rows: &[T],
    query: &ListQuery,
    project: impl Fn(&T) -> serde_json::Value,
) -> ListEnvelopeDto {
    let selected = query.select.as_slice();
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            apply_select(
                project(row),
                if selected.is_empty() {
                    None
                } else {
                    Some(selected)
                },
            )
        })
        .collect();
    ListEnvelopeDto {
        items,
        page_info: PageMetaDto {
            limit: query.top.unwrap_or_default(),
            skip: query.skip,
        },
    }
}

/// JSON projection of a row, used by the list envelope.
fn to_value<T: serde::Serialize>(row: &T) -> serde_json::Value {
    serde_json::to_value(row).unwrap_or(serde_json::Value::Null)
}

/// A `201 Created` response with its `Location` and the gateway error-source
/// header.
fn created<T: serde::Serialize>(location: &str, body: &T) -> Response {
    with_error_source(
        (
            StatusCode::CREATED,
            [(axum::http::header::LOCATION, location.to_owned())],
            Json(body),
        )
            .into_response(),
    )
}

/// A `200 OK` response carrying the gateway error-source header.
fn ok<T: serde::Serialize>(body: T) -> Response {
    with_error_source((StatusCode::OK, Json(body)).into_response())
}

/// A `204 No Content` response carrying the gateway error-source header.
fn no_content() -> Response {
    with_error_source(StatusCode::NO_CONTENT.into_response())
}

// -------------------------------------------------------------- upstreams

/// `POST /upstreams`
///
/// # Errors
/// Returns an OAGW problem for validation failures (`400`), an alias conflict
/// (`409`) or an authorization failure (`403`).
pub async fn create_upstream(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Json(body): Json<UpstreamRequestDto>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let upstream = svc
        .create_upstream(&ctx, body.into_command())
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let location = format!(
        "{}/{}",
        uri.path().trim_end_matches('/'),
        crate::domain::model::resource_gts_id(crate::domain::model::UPSTREAM_TYPE, upstream.id)
    );
    Ok(created(
        &location,
        &UpstreamResponseDto::from_row(&upstream),
    ))
}

/// `GET /upstreams`
///
/// # Errors
/// Returns an OAGW problem for a malformed list query (`400`) or an
/// authorization failure (`403`).
pub async fn list_upstreams(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let query = build_list_query(
        query.as_deref(),
        svc.default_page_size(),
        svc.max_page_size(),
    )
    .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let rows = svc
        .list_upstreams(&ctx, &query)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(envelope(
        &rows,
        &query,
        |row: &crate::domain::model::Upstream| to_value(&UpstreamResponseDto::from_row(row)),
    )))
}

/// `GET /upstreams/{id}`
///
/// # Errors
/// Returns an OAGW problem for a malformed id (`400`), an upstream outside the
/// tenant (`404`) or an authorization failure (`403`).
pub async fn get_upstream(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let id = parse_upstream_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    let upstream = svc
        .get_upstream(&ctx, id)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(UpstreamResponseDto::from_row(&upstream)))
}

/// `PUT /upstreams/{id}`
///
/// # Errors
/// Returns an OAGW problem for validation failures (`400`), an alias conflict
/// (`409`) or an authorization failure (`403`).
pub async fn replace_upstream(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpstreamRequestDto>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let id = parse_upstream_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    let upstream = svc
        .replace_upstream(&ctx, id, body.into_command())
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(UpstreamResponseDto::from_row(&upstream)))
}

/// `DELETE /upstreams/{id}`
///
/// # Errors
/// Returns an OAGW problem for an upstream outside the tenant (`404`) or an
/// authorization failure (`403`).
pub async fn delete_upstream(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let id = parse_upstream_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    svc.delete_upstream(&ctx, id)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(no_content())
}

// ----------------------------------------------------------------- routes

/// `POST /routes`
///
/// # Errors
/// Returns an OAGW problem for validation failures (`400`), a match-rule
/// collision (`409`) or an authorization failure (`403`).
pub async fn create_route(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Json(body): Json<RouteRequestDto>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let command = body
        .into_command()
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let route = svc
        .create_route(&ctx, command)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let location = format!(
        "{}/{}",
        uri.path().trim_end_matches('/'),
        crate::domain::model::resource_gts_id(crate::domain::model::ROUTE_TYPE, route.id)
    );
    Ok(created(&location, &RouteResponseDto::from_row(&route)))
}

/// `GET /routes`
///
/// # Errors
/// Returns an OAGW problem for a malformed list query (`400`) or an
/// authorization failure (`403`).
pub async fn list_routes(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let query = build_list_query(
        query.as_deref(),
        svc.default_page_size(),
        svc.max_page_size(),
    )
    .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let rows = svc
        .list_routes(&ctx, &query)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(envelope(
        &rows,
        &query,
        |row: &crate::domain::model::Route| to_value(&RouteResponseDto::from_row(row)),
    )))
}

/// `GET /routes/{id}`
///
/// # Errors
/// Returns an OAGW problem for a malformed id (`400`), a route outside the
/// tenant (`404`) or an authorization failure (`403`).
pub async fn get_route(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let id = parse_route_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    let route = svc
        .get_route(&ctx, id)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(RouteResponseDto::from_row(&route)))
}

/// `PUT /routes/{id}`
///
/// # Errors
/// Returns an OAGW problem for validation failures (`400`), a match-rule
/// collision (`409`) or an authorization failure (`403`).
pub async fn replace_route(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RouteRequestDto>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let id = parse_route_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    let command = body
        .into_command()
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let route = svc
        .replace_route(&ctx, id, command)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(RouteResponseDto::from_row(&route)))
}

/// `DELETE /routes/{id}`
///
/// # Errors
/// Returns an OAGW problem for a route outside the tenant (`404`) or an
/// authorization failure (`403`).
pub async fn delete_route(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let id = parse_route_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    svc.delete_route(&ctx, id)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(no_content())
}

// ---------------------------------------------------------------- plugins

/// `POST /plugins`
///
/// # Errors
/// Returns an OAGW problem for a malformed source (`400`), a name conflict
/// (`409`) or an authorization failure (`403`).
pub async fn create_plugin(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Json(body): Json<PluginRequestDto>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let plugin = svc
        .create_plugin(&ctx, body.into_command())
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let location = format!(
        "{}/{}",
        uri.path().trim_end_matches('/'),
        crate::domain::model::resource_gts_id(plugin.plugin_type.gts_base_type(), plugin.id)
    );
    Ok(created(&location, &PluginResponseDto::from_row(&plugin)))
}

/// `GET /plugins`
///
/// # Errors
/// Returns an OAGW problem for a malformed list query (`400`) or an
/// authorization failure (`403`).
pub async fn list_plugins(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let query = build_list_query(
        query.as_deref(),
        svc.default_page_size(),
        svc.max_page_size(),
    )
    .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let rows = svc
        .list_plugins(&ctx, &query, None)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(envelope(
        &rows,
        &query,
        crate::domain::services::plugin_document,
    )))
}

/// `GET /plugins/{id}`
///
/// # Errors
/// Returns an OAGW problem for a malformed id (`400`), a plugin outside the
/// tenant (`404`) or an authorization failure (`403`).
pub async fn get_plugin(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let uuid = parse_plugin_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    let plugin = svc
        .get_plugin(&ctx, uuid, &id)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(ok(PluginResponseDto::from_row(&plugin)))
}

/// `GET /plugins/{id}/source` — the Starlark source of a custom plugin.
///
/// # Errors
/// Same as [`get_plugin`].
pub async fn get_plugin_source(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let uuid = parse_plugin_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    let plugin = svc
        .get_plugin(&ctx, uuid, &id)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    let dto = PluginSourceResponseDto {
        id: crate::domain::model::resource_gts_id(plugin.plugin_type.gts_base_type(), plugin.id),
        plugin_type: plugin.plugin_type,
        source_code: plugin.source_code,
    };
    Ok(with_error_source(
        (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                format!("{}; charset=utf-8", super::dto::STARLARK_MEDIA_TYPE),
            )],
            Json(dto),
        )
            .into_response(),
    ))
}

/// `DELETE /plugins/{id}`
///
/// Returns `409 PluginInUse` with `plugin_id` and `referenced_by` when an
/// upstream or a route still binds the plugin.
///
/// # Errors
/// Returns an OAGW problem for a plugin outside the tenant (`404`), an in-use
/// plugin (`409`) or an authorization failure (`403`).
pub async fn delete_plugin(
    uri: axum::http::Uri,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(security): Extension<SecurityContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let ctx = request_context(&uri, &headers, &security)?;
    let uuid = parse_plugin_id(&id).map_err(|error| stamped(&uri, &headers, error.into()))?;
    svc.delete_plugin(&ctx, uuid, &id)
        .await
        .map_err(|error| stamped(&uri, &headers, error.into()))?;
    Ok(no_content())
}

// ------------------------------------------------------------------ proxy

/// `ANY /proxy/{alias}` — proxy a request to the upstream its alias names.
///
/// Every method is served, because the accepted set is a property of the
/// matched route, not of the gateway path.
///
/// # Errors
/// Returns an OAGW problem for a caller without a tenant identity (`403`), a
/// malformed request (`400`), an unroutable alias (`404`), a refused quota or
/// origin (`429`/`403`), or an upstream the gateway could not reach
/// (`502`/`503`/`504`).
pub async fn proxy(
    uri: axum::http::Uri,
    Path(alias): Path<String>,
    Extension(engine): Extension<Arc<ProxyEngine>>,
    Extension(security): Extension<SecurityContext>,
    request: axum::extract::Request,
) -> Result<Response, OagwProblem> {
    proxy_request(&uri, &alias, None, &engine, &security, request).await
}

/// `ANY /proxy/{alias}/{*path_suffix}` — proxy a request carrying a suffix.
///
/// # Errors
/// As [`proxy`].
pub async fn proxy_with_suffix(
    uri: axum::http::Uri,
    Path((alias, suffix)): Path<(String, String)>,
    Extension(engine): Extension<Arc<ProxyEngine>>,
    Extension(security): Extension<SecurityContext>,
    request: axum::extract::Request,
) -> Result<Response, OagwProblem> {
    proxy_request(&uri, &alias, Some(&suffix), &engine, &security, request).await
}

/// The shared body of both proxy entry points.
///
/// The request is bound into a [`crate::domain::dto::ProxyContext`] — the
/// context the auth, guard and transform plugins of `ADR`-0002 consume — and
/// handed to the data plane, which resolves the alias, selects the route and
/// streams the answer back. A WebSocket upgrade is tunnelled rather than
/// forwarded: both sides of the connection are spliced together once the
/// upstream answers `101`.
async fn proxy_request(
    uri: &axum::http::Uri,
    alias: &str,
    suffix: Option<&str>,
    engine: &ProxyEngine,
    security: &SecurityContext,
    mut request: axum::extract::Request,
) -> Result<Response, OagwProblem> {
    let headers = request.headers().clone();

    // A CORS preflight is answered here and never proxied (`ADR`-0004, preflight
    // request handling): a browser sends no credentials, so there is no tenant
    // context to resolve an upstream with, and the origin is enforced on the
    // actual request that follows instead.
    if crate::infra::proxy::cors::is_preflight(
        request.method().as_str(),
        &inbound_headers(request.headers()),
    ) {
        return Ok(preflight_answer(request.version(), &headers));
    }

    // The data plane resolves routes inside the caller's tenant, so an
    // anonymous caller is refused before any routing happens.
    let ctx = request_context(uri, &headers, security)?;

    // The declared body size is checked before the first byte is read, so an
    // oversized payload is never buffered (`DESIGN` §2.2).
    crate::infra::proxy::body::validate(&inbound_headers(&headers))
        .map_err(|error| stamped(uri, &headers, error.into()))?;

    let is_upgrade = crate::infra::proxy::headers::is_upgrade(&headers);
    let on_upgrade = if is_upgrade {
        Some(
            request
                .extensions_mut()
                .remove::<hyper::upgrade::OnUpgrade>()
                .ok_or_else(|| {
                    stamped(
                        uri,
                        &headers,
                        DomainError::validation(
                            "the connection does not support the requested protocol upgrade",
                        )
                        .into(),
                    )
                })?,
        )
    } else {
        None
    };

    let target_host = request
        .headers()
        .get(crate::api::rest::error::HEADER_X_OAGW_TARGET_HOST)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    // The path the route matcher sees is the suffix the alias-only form does
    // not carry: `proxy_context` already strips the alias from it.
    let context = super::dto::proxy_context(
        alias,
        suffix,
        request.method(),
        super::dto::query_of(request.uri().query()),
        request.headers(),
        extract_trace_id(request.headers()),
        (ctx.tenant, security.subject_id()),
    );

    let method = request.method().clone();
    let body = axum::body::Body::from_stream(crate::infra::proxy::body::Bounded::new(
        request.into_body().into_data_stream(),
    ));

    match engine
        .forward(ForwardRequest {
            context,
            method,
            target_host,
            body,
            upgrade: is_upgrade,
        })
        .await
    {
        Ok(ForwardOutcome::Response(response)) => Ok(stamp_upstream_source(response)),
        Ok(ForwardOutcome::Tunnel { parts, stream }) => {
            tunnel(on_upgrade, &parts, stream).map_err(|problem| stamped(uri, &headers, problem))
        }
        Err(error) => Err(stamped(uri, &headers, error.into())),
    }
}

/// The headers of an inbound request, as the map the body checks read.
fn inbound_headers(headers: &HeaderMap) -> std::collections::BTreeMap<String, String> {
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

/// Stamp `X-OAGW-Error-Source: upstream` on a response the upstream produced
/// (`ADR`-0007): the gateway did not generate it, so a client knows the body is
/// the upstream's own.
fn stamp_upstream_source(mut response: Response) -> Response {
    response.headers_mut().insert(
        crate::api::rest::error::HEADER_X_OAGW_ERROR_SOURCE,
        axum::http::HeaderValue::from_static(crate::api::rest::error::ERROR_SOURCE_UPSTREAM),
    );
    response
}

/// The permissive preflight answer (`ADR`-0004, preflight request handling):
/// `204` echoing the origin, method and headers the browser asked for, with
/// `Access-Control-Max-Age` and a `Vary` that keeps a shared cache honest.
fn preflight_answer(version: axum::http::Version, headers: &HeaderMap) -> Response {
    let answer = crate::infra::proxy::cors::preflight(&inbound_headers(headers));
    let mut builder = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .version(version);
    for (name, value) in answer.into_headers() {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(&value),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::NO_CONTENT.into_response())
}

/// Splice the caller's half of an upgrade onto the upstream's.
///
/// The caller's [`hyper::upgrade::OnUpgrade`] was taken from the request
/// extensions, exactly as `axum::extract::ws::WebSocketUpgrade` does, and is
/// completed by returning the `101` the upstream produced: the transport layer
/// answers the client handshake while this task copies bytes between the two
/// raw connections until either side closes. The response therefore carries no
/// extension of its own.
fn tunnel(
    on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    parts: &axum::http::response::Parts,
    upstream: crate::infra::proxy::transport::TunnelStream,
) -> Result<Response, OagwProblem> {
    let on_upgrade = on_upgrade.ok_or_else(|| {
        protocol_problem("the connection does not support the requested protocol upgrade")
    })?;
    let response = tunnel_response(parts)?;
    tokio::spawn(tunnel_copy(on_upgrade, upstream));
    Ok(stamp_upstream_source(response))
}

/// The `101` the caller receives, built from the upstream handshake.
fn tunnel_response(parts: &axum::http::response::Parts) -> Result<Response, OagwProblem> {
    let mut builder = Response::builder()
        .status(parts.status)
        .version(parts.version);
    for (name, value) in &parts.headers {
        builder = builder.header(name, value);
    }
    builder.body(axum::body::Body::empty()).map_err(|error| {
        protocol_problem(format!(
            "the upstream handshake is not a valid HTTP response: {error}"
        ))
    })
}

/// A `502` protocol problem of the upgrade path.
fn protocol_problem(detail: impl Into<String>) -> OagwProblem {
    OagwProblem::new(OagwErrorType::ProtocolError, detail)
}

/// Complete the caller's upgrade, then copy bytes both ways until either side
/// of the tunnel closes.
async fn tunnel_copy(
    caller: hyper::upgrade::OnUpgrade,
    upstream: crate::infra::proxy::transport::TunnelStream,
) {
    let Ok(stream) = caller.await else {
        tracing::warn!("the caller did not complete the upgrade handshake");
        return;
    };
    let mut caller = hyper_util::rt::TokioIo::new(stream);
    let mut upstream = hyper_util::rt::TokioIo::new(upstream);
    let outcome = tokio::io::copy_bidirectional(&mut caller, &mut upstream).await;
    log_tunnel_outcome(outcome);
}

/// Record how a tunnel ended.
fn log_tunnel_outcome(outcome: std::io::Result<(u64, u64)>) {
    match outcome {
        Ok((client_bytes, upstream_bytes)) => {
            tracing::debug!(client_bytes, upstream_bytes, "websocket tunnel closed");
        }
        Err(error) => tracing::warn!(diagnostic = %error, "websocket tunnel aborted"),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "handlers_tests.rs"]
mod tests;
