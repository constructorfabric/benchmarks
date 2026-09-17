//! REST handlers for the OAGW gear (DESIGN §3.3 "Management API" and
//! "Proxy API").
//!
//! Management handlers are thin: they extract the request, delegate to
//! [`ControlPlaneService`], and shape the response. The proxy handler owns
//! the transport-level responsibilities the data plane cannot see — CORS
//! preflight short-circuit (ADR-0004) and body validation (size limit,
//! content-length match, transfer-encoding) — before building the
//! [`ProxyRequest`] and invoking the data plane.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, Request};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::dto::{ListParams, ProxyIdentity, ProxyRequest};
use crate::domain::error::{OagwError, OagwResult};
use crate::domain::models::{Plugin, Route, Upstream};
use crate::domain::services::DataPlaneService;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::DataPlaneServiceImpl;

use super::dto::{ListBody, ListQuery, PluginSourceBody};

/// Handler result: `Ok` response or an [`OagwError`] rendered as a gateway
/// problem (or passthrough) response.
pub type ApiResult<T> = Result<T, OagwError>;

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams` — create an upstream.
///
/// # Errors
///
/// `400` validation, `403` missing scope/bind permission, `409` alias
/// conflict.
pub async fn create_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<Upstream>,
) -> ApiResult<impl IntoResponse> {
    let stored = cp.create_upstream(&ctx, body).await?;
    let id = stored.record.id.unwrap_or_else(Uuid::nil);
    let location = format!("/oagw/v1/upstreams/{id}");
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(stored.record),
    ))
}

/// `GET /oagw/v1/upstreams` — list upstreams (OData query parameters).
///
/// # Errors
///
/// `400` on unsupported query expressions, `403` on missing scope.
pub async fn list_upstreams(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListBody>> {
    let params: ListParams = query.into_domain()?;
    let (items, total) = cp.list_upstreams(&ctx, &params).await?;
    let records = items
        .into_iter()
        .map(|s| serde_json::to_value(s.record).unwrap_or_default())
        .collect();
    Ok(Json(ListBody::from_records(
        records,
        total,
        params.select.as_deref(),
    )))
}

/// `GET /oagw/v1/upstreams/{id}` — get an upstream by id.
///
/// # Errors
///
/// `404` when missing or owned by another tenant, `403` on missing scope.
pub async fn get_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Upstream>> {
    let stored = cp.get_upstream(&ctx, id).await?;
    Ok(Json(stored.record))
}

/// `PUT /oagw/v1/upstreams/{id}` — replace an upstream.
///
/// # Errors
///
/// `400` validation/alias immutability, `403` missing scope, `404` missing or
/// foreign.
pub async fn replace_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<Upstream>,
) -> ApiResult<Json<Upstream>> {
    let stored = cp.replace_upstream(&ctx, id, body).await?;
    Ok(Json(stored.record))
}

/// `DELETE /oagw/v1/upstreams/{id}` — delete an upstream (cascades routes).
///
/// # Errors
///
/// `403` on missing scope, `404` when missing or foreign.
pub async fn delete_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    cp.delete_upstream(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes` — create a route.
///
/// # Errors
///
/// `400` validation, `403` missing scope, `404` upstream not owned, `409`
/// match conflict.
pub async fn create_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<Route>,
) -> ApiResult<impl IntoResponse> {
    let stored = cp.create_route(&ctx, body).await?;
    let id = stored.record.id.unwrap_or_else(Uuid::nil);
    let location = format!("/oagw/v1/routes/{id}");
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(stored.record),
    ))
}

/// `GET /oagw/v1/routes` — list routes (OData query parameters).
///
/// # Errors
///
/// `400` on unsupported query expressions, `403` on missing scope.
pub async fn list_routes(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListBody>> {
    let params: ListParams = query.into_domain()?;
    let (items, total) = cp.list_routes(&ctx, &params).await?;
    let records = items
        .into_iter()
        .map(|s| serde_json::to_value(s.record).unwrap_or_default())
        .collect();
    Ok(Json(ListBody::from_records(
        records,
        total,
        params.select.as_deref(),
    )))
}

/// `GET /oagw/v1/routes/{id}` — get a route by id.
///
/// # Errors
///
/// `404` when missing or foreign, `403` on missing scope.
pub async fn get_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Route>> {
    let stored = cp.get_route(&ctx, id).await?;
    Ok(Json(stored.record))
}

/// `PUT /oagw/v1/routes/{id}` — replace a route (`upstream_id` immutable).
///
/// # Errors
///
/// `400` validation, `403` missing scope, `404` missing or foreign, `409`
/// match conflict.
pub async fn replace_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<Route>,
) -> ApiResult<Json<Route>> {
    let stored = cp.replace_route(&ctx, id, body).await?;
    Ok(Json(stored.record))
}

/// `DELETE /oagw/v1/routes/{id}` — delete a route.
///
/// # Errors
///
/// `403` on missing scope, `404` when missing or foreign.
pub async fn delete_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    cp.delete_route(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugins (custom, UUID-backed)
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins` — create a custom (Starlark) plugin.
///
/// # Errors
///
/// `400` validation, `403` missing family scope, `409` name conflict.
pub async fn create_plugin(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<Plugin>,
) -> ApiResult<impl IntoResponse> {
    let stored = cp.create_plugin(&ctx, body).await?;
    let id = stored.record.id.unwrap_or_else(Uuid::nil);
    let location = format!("/oagw/v1/plugins/{id}");
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(stored.record),
    ))
}

/// `GET /oagw/v1/plugins` — list plugins visible to the token.
///
/// # Errors
///
/// `400` on unsupported query expressions.
pub async fn list_plugins(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListBody>> {
    let params: ListParams = query.into_domain()?;
    let (items, total) = cp.list_plugins(&ctx, &params).await?;
    let records = items
        .into_iter()
        .map(|s| serde_json::to_value(s.record).unwrap_or_default())
        .collect();
    Ok(Json(ListBody::from_records(
        records,
        total,
        params.select.as_deref(),
    )))
}

/// `GET /oagw/v1/plugins/{id}` — get a custom plugin by id.
///
/// # Errors
///
/// `403` on missing family read scope, `404` when missing or foreign.
pub async fn get_plugin(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Plugin>> {
    let stored = cp.get_plugin(&ctx, id).await?;
    Ok(Json(stored.record))
}

/// `GET /oagw/v1/plugins/{id}/source` — get the Starlark source.
///
/// # Errors
///
/// `403` on missing family read scope, `404` when missing or foreign.
pub async fn get_plugin_source(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<PluginSourceBody>> {
    let source = cp.get_plugin_source(&ctx, id).await?;
    Ok(Json(PluginSourceBody { source }))
}

/// `DELETE /oagw/v1/plugins/{id}` — delete a custom plugin.
///
/// # Errors
///
/// `403` on missing family scope, `404` when missing or foreign, `409` when
/// referenced by an upstream or route binding.
pub async fn delete_plugin(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    cp.delete_plugin(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Proxy (data plane)
// ---------------------------------------------------------------------------

/// `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?query]` — proxy a request.
///
/// Handles both path forms (`{alias}` and `{alias}/{*path}`) from the raw
/// request URI so the path suffix is never double-encoded. CORS preflight is
/// answered here with a permissive 204 (ADR-0004); actual cross-origin
/// validation happens in the data plane after upstream resolution.
///
/// # Errors
///
/// Maps every data-plane failure to a gateway problem response; upstream
/// responses (including errors) pass through unchanged with
/// `X-OAGW-Error-Source: upstream`.
pub async fn proxy(
    Extension(dp): Extension<Arc<DataPlaneServiceImpl>>,
    Extension(ctx): Extension<SecurityContext>,
    req: Request,
) -> OagwResult<crate::domain::dto::ProxyResponse> {
    let method = req.method().clone();
    let raw_path = req.uri().path().to_owned();

    // The alias is split from the raw URI (not an axum `Path` extractor):
    // the operation registers both the bare `{alias}` and the wildcard
    // `{alias}/{*path}` forms, and a single-value `Path<String>` would
    // reject the two-parameter form with a 500 while a tuple extractor
    // would reject the bare form.
    let (alias, path_suffix) = split_proxy_path(&raw_path);

    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();

    // CORS preflight: OPTIONS + Origin + Access-Control-Request-Method →
    // permissive 204 before any upstream resolution (ADR-0004).
    if crate::infra::cors::is_preflight(&method, &headers) {
        return Ok(crate::infra::cors::preflight_response(&headers));
    }

    // Body validation (DESIGN "Body Validation Rules"): transfer-encoding,
    // content-length match, hard size limit before buffering.
    let (parts, body) = req.into_parts();
    validate_transfer_encoding(&headers)?;
    let bytes = axum::body::to_bytes(body, dp.body_limit())
        .await
        .map_err(|_| OagwError::PayloadTooLarge {
            limit: dp.body_limit(),
        })?;
    validate_content_length(&headers, bytes.len())?;

    let query: Vec<(String, String)> = parts
        .uri
        .query()
        .map(|q| {
            form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    let identity = ProxyIdentity {
        subject_id: ctx.subject_id(),
        tenant_id: ctx.subject_tenant_id(),
    };
    let proxy_req = ProxyRequest {
        method,
        path_suffix: path_suffix.to_owned(),
        query,
        headers,
        body: Some(bytes),
    };
    dp.execute_proxy(&ctx, identity, &alias, proxy_req).await
}

/// Split `/oagw/v1/proxy/<alias>[/<path…>]` into `(alias, path_suffix)`.
///
/// The suffix keeps its leading `/` and is preserved verbatim (never decoded
/// or re-encoded); the bare form yields an empty suffix.
fn split_proxy_path(raw_path: &str) -> (&str, &str) {
    let rest = raw_path.strip_prefix("/oagw/v1/proxy/").unwrap_or_default();
    match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    }
}

/// Reject unsupported `Transfer-Encoding` values (only `chunked` allowed).
fn validate_transfer_encoding(headers: &[(String, String)]) -> OagwResult<()> {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("transfer-encoding")
            && !value.trim().eq_ignore_ascii_case("chunked")
        {
            return Err(OagwError::Validation {
                detail: format!("unsupported transfer-encoding {value:?}"),
            });
        }
    }
    Ok(())
}

/// When `Content-Length` is present it must be a valid integer equal to the
/// buffered body size.
fn validate_content_length(headers: &[(String, String)], actual: usize) -> OagwResult<()> {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            let expected: usize = value.trim().parse().map_err(|_| OagwError::Validation {
                detail: format!("invalid content-length value {value:?}"),
            })?;
            if expected != actual {
                return Err(OagwError::Validation {
                    detail: format!(
                        "content-length {expected} does not match actual body size {actual}"
                    ),
                });
            }
        }
    }
    Ok(())
}
