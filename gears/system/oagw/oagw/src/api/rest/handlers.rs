//! REST handlers of the OAGW control plane (upstream, route and plugin CRUD)
//! and of the data plane (the `/oagw/v1/proxy/{alias}` request path).
//!
//! The handlers are thin: they decode the transport, extract the tenant from
//! the [`SecurityContext`] and delegate every rule to the domain layer — the
//! control plane for the management operations, [`DataPlaneService`] for the
//! proxy. The `X-OAGW-Error-Source: gateway` header (ADR-0007) is stamped on
//! the success responses here and on the error responses by
//! [`crate::error::OagwError`], which also carries the request path as the
//! RFC 9457 `instance` member.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, Request};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::domain::control_plane::{ControlPlane, PluginRecord, RouteRecord, UpstreamRecord};
use crate::domain::model::{
    PluginSpec, ROUTE_ID_PREFIX, RouteSpec, UPSTREAM_ID_PREFIX, UpstreamSpec, parse_plugin_id,
    parse_resource_id, route_gts_id, upstream_gts_id,
};
use crate::domain::proxy::DataPlaneService;
use crate::error::{ApiResult, OagwError, with_error_source};

use super::dto::{ListFilter, list_params};

/// Stamp `X-OAGW-Error-Source: gateway` on a success response: ADR-0007 puts
/// the header on every OAGW response, successes included.
fn gateway_response(response: impl IntoResponse) -> Response {
    with_error_source(response.into_response())
}

/// Attach the request path as the RFC 9457 `instance` member of any gateway
/// error `body` produces.
async fn with_instance<T>(path: &str, body: impl Future<Output = ApiResult<T>>) -> ApiResult<T> {
    body.await.map_err(|error| error.with_instance(path))
}

/// Read a request body as a JSON document, enforcing the configured limit.
///
/// The body is read chunk by chunk with the limit applied *before* each chunk is
/// buffered (DESIGN "Body Validation Rules": "Max size | Hard limit 100MB;
/// reject before buffering | `413 PayloadTooLarge`"), and parsed manually
/// instead of through `Json<T>` so that a schema-level rejection (unknown
/// member, missing member, non-numeric port) becomes an OAGW problem document
/// instead of a transport rejection. Reading the stream by hand also keeps the
/// 413 distinguishable from a transport failure, which `to_bytes` folds into one
/// error.
///
/// # Errors
///
/// Returns a 413 [`OagwError`] when the body exceeds the configured limit, a 400
/// when it cannot be read, is empty or is not a JSON document.
async fn read_json(request: Request, limit: usize) -> Result<Value, OagwError> {
    let mut stream = request.into_body().into_data_stream();
    let mut buffered: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            OagwError::validation(format!("the body could not be read: {error}"))
        })?;
        if buffered.len() + chunk.len() > limit {
            return Err(OagwError::payload_too_large(format!(
                "the request body exceeds the configured limit of {limit} bytes"
            )));
        }
        buffered.extend_from_slice(&chunk);
    }
    if buffered.is_empty() {
        return Err(OagwError::validation("a JSON document is required"));
    }
    serde_json::from_slice(&buffered).map_err(|error| {
        OagwError::validation(format!("the request body is not a JSON document: {error}"))
    })
}

/// Decode a JSON document into a resource DTO.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] describing the first schema violation.
fn decode<T: DeserializeOwned>(document: &Value, resource: &str) -> Result<T, OagwError> {
    serde_json::from_value(document.clone()).map_err(|error| {
        OagwError::validation(format!(
            "the request body is not a valid {resource} document: {error}"
        ))
    })
}

/// `POST /oagw/v1/upstreams`
///
/// Creates an upstream for the calling tenant: the alias is derived from the
/// endpoints (or validated against the derivation) and reserved per tenant.
///
/// # Errors
///
/// Returns a gateway problem document when the body is not a valid upstream
/// document (400), when the alias rules are violated (400) or when the alias is
/// already taken (409).
pub async fn create_upstream(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(config): Extension<OagwConfig>,
    uri: Uri,
    request: Request,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let document = read_json(request, config.body_limit_bytes).await?;
        let spec: UpstreamSpec = decode(&document, "upstream")?;
        let record = plane.create_upstream(ctx.subject_tenant_id(), spec)?;
        let location = format!(
            "{}/{}",
            uri.path().trim_end_matches('/'),
            upstream_gts_id(record.id)
        );
        Ok(gateway_response((
            StatusCode::CREATED,
            [(axum::http::header::LOCATION, location)],
            Json(record.wire()),
        )))
    })
    .await
}

/// `GET /oagw/v1/upstreams`
///
/// Lists the upstreams of the calling tenant, ordered by alias, honoring
/// `$top` / `$skip`.
///
/// # Errors
///
/// Returns a gateway problem document when the list cannot be served (500).
pub async fn list_upstreams(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let (params, filter) = list_params(&query)?;
        let bodies = wire_page(
            &plane.list_upstreams(ctx.subject_tenant_id(), params.top, params.skip),
            UpstreamRecord::wire,
            filter.as_ref(),
        );
        Ok(gateway_response((StatusCode::OK, Json(bodies))))
    })
    .await
}

/// Render one page of records as their wire documents, applying an `OData`
/// `$filter` before the page is cut: the filter selects across the whole
/// collection, `$top` / `$skip` page the selection.
fn wire_page<R, T>(records: &[R], wire: fn(&R) -> T, filter: Option<&ListFilter>) -> Vec<T>
where
    T: Serialize,
{
    let documents: Vec<T> = records.iter().map(wire).collect();
    match filter {
        None => documents,
        Some(filter) => documents
            .into_iter()
            .filter(|document| {
                serde_json::to_value(document).is_ok_and(|value| filter.matches(&value))
            })
            .collect(),
    }
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed (400)
/// or when the tenant owns no such upstream (404).
pub async fn get_upstream(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_resource_id(UPSTREAM_ID_PREFIX, &id)?;
        let record = plane.get_upstream(ctx.subject_tenant_id(), id)?;
        Ok(gateway_response((StatusCode::OK, Json(record.wire()))))
    })
    .await
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// Replaces the stored document. The alias is immutable, so an endpoint change
/// that would change the derived alias is rejected.
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed or the
/// body is invalid (400), when the alias rules are violated (400) or when the
/// tenant owns no such upstream (404). A `PUT` never creates.
pub async fn put_upstream(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(config): Extension<OagwConfig>,
    uri: Uri,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_resource_id(UPSTREAM_ID_PREFIX, &id)?;
        let document = read_json(request, config.body_limit_bytes).await?;
        let spec: UpstreamSpec = decode(&document, "upstream")?;
        let record = plane.update_upstream(ctx.subject_tenant_id(), id, spec)?;
        Ok(gateway_response((StatusCode::OK, Json(record.wire()))))
    })
    .await
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed (400)
/// or when the tenant owns no such upstream (404).
pub async fn delete_upstream(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_resource_id(UPSTREAM_ID_PREFIX, &id)?;
        plane.delete_upstream(ctx.subject_tenant_id(), id)?;
        Ok(gateway_response(StatusCode::NO_CONTENT))
    })
    .await
}

/// `POST /oagw/v1/routes`
///
/// Creates a route of an upstream owned by the calling tenant.
///
/// # Errors
///
/// Returns a gateway problem document when the body is not a valid route
/// document or references no upstream of the tenant (400) and when an existing
/// route of the upstream already matches the same path for an overlapping
/// method set (409, `reason: ROUTE_MATCH_CONFLICT`).
pub async fn create_route(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(config): Extension<OagwConfig>,
    uri: Uri,
    request: Request,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let document = read_json(request, config.body_limit_bytes).await?;
        let spec: RouteSpec = decode(&document, "route")?;
        let record = plane.create_route(ctx.subject_tenant_id(), spec)?;
        let location = format!(
            "{}/{}",
            uri.path().trim_end_matches('/'),
            route_gts_id(record.id)
        );
        Ok(gateway_response((
            StatusCode::CREATED,
            [(header::LOCATION, location)],
            Json(record.wire()),
        )))
    })
    .await
}

/// `GET /oagw/v1/routes`
///
/// Lists the routes of the calling tenant, ordered by upstream then match path,
/// honoring `$top` / `$skip`.
///
/// # Errors
///
/// Returns a gateway problem document when the list cannot be served (500).
pub async fn list_routes(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let (params, filter) = list_params(&query)?;
        let bodies = wire_page(
            &plane.list_routes(ctx.subject_tenant_id(), params.top, params.skip),
            RouteRecord::wire,
            filter.as_ref(),
        );
        Ok(gateway_response((StatusCode::OK, Json(bodies))))
    })
    .await
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed (400)
/// or when the tenant owns no such route (404).
pub async fn get_route(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_resource_id(ROUTE_ID_PREFIX, &id)?;
        let record = plane.get_route(ctx.subject_tenant_id(), id)?;
        Ok(gateway_response((StatusCode::OK, Json(record.wire()))))
    })
    .await
}

/// `PUT /oagw/v1/routes/{id}`
///
/// Replaces the stored route document. `upstream_id` is immutable and a `PUT`
/// never creates.
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed or the
/// body is invalid (400), when the body moves the route to another upstream
/// (400) or when the replacement overlaps an existing route (409).
pub async fn put_route(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(config): Extension<OagwConfig>,
    uri: Uri,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_resource_id(ROUTE_ID_PREFIX, &id)?;
        let document = read_json(request, config.body_limit_bytes).await?;
        let spec: RouteSpec = decode(&document, "route")?;
        let record = plane.update_route(ctx.subject_tenant_id(), id, spec)?;
        Ok(gateway_response((StatusCode::OK, Json(record.wire()))))
    })
    .await
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed (400)
/// or when the tenant owns no such route (404).
pub async fn delete_route(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_resource_id(ROUTE_ID_PREFIX, &id)?;
        plane.delete_route(ctx.subject_tenant_id(), id)?;
        Ok(gateway_response(StatusCode::NO_CONTENT))
    })
    .await
}

/// `POST /oagw/v1/plugins`
///
/// Registers a custom (Starlark) plugin for the calling tenant, or names a
/// registry plugin when the document carries no `source`.
///
/// # Errors
///
/// Returns a gateway problem document when the body is not a valid plugin
/// document (400) and when the tenant already owns a plugin with this name
/// (409, `reason: PLUGIN_NAME_CONFLICT`).
pub async fn create_plugin(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(config): Extension<OagwConfig>,
    uri: Uri,
    request: Request,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let document = read_json(request, config.body_limit_bytes).await?;
        let spec: PluginSpec = decode(&document, "plugin")?;
        let record = plane.create_plugin(ctx.subject_tenant_id(), spec)?;
        let location = format!("{}/{}", uri.path().trim_end_matches('/'), record.plugin_ref);
        Ok(gateway_response((
            StatusCode::CREATED,
            [(header::LOCATION, location)],
            Json(record.wire()),
        )))
    })
    .await
}

/// `GET /oagw/v1/plugins`
///
/// Lists the plugins of the calling tenant, ordered by name, honoring `$top` /
/// `$skip`.
///
/// # Errors
///
/// Returns a gateway problem document when the list cannot be served (500).
pub async fn list_plugins(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let (params, filter) = list_params(&query)?;
        let bodies = wire_page(
            &plane.list_plugins(ctx.subject_tenant_id(), params.top, params.skip),
            PluginRecord::wire,
            filter.as_ref(),
        );
        Ok(gateway_response((StatusCode::OK, Json(bodies))))
    })
    .await
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed (400)
/// or when the tenant owns no such plugin (404).
pub async fn get_plugin(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_plugin_id(&id)?;
        let record = plane.get_plugin(ctx.subject_tenant_id(), id)?;
        Ok(gateway_response((StatusCode::OK, Json(record.wire()))))
    })
    .await
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// Returns the Starlark source of the plugin as `text/plain; charset=utf-8`.
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed (400),
/// when the tenant owns no such plugin (404) or when the plugin is a named
/// registry plugin and therefore has no source document (503
/// `plugin.not_found.v1`, the type DESIGN documents for a plugin that cannot be
/// resolved).
pub async fn get_plugin_source(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_plugin_id(&id)?;
        let record = plane.get_plugin(ctx.subject_tenant_id(), id)?;
        let source = record.spec.source.as_ref().ok_or_else(|| {
            OagwError::plugin_not_found(format!(
                "plugin `{}` has no source document: it is a named registry plugin",
                record.plugin_ref
            ))
        })?;
        Ok(gateway_response((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            source.clone(),
        )))
    })
    .await
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// Deletes a plugin that no upstream and no route still binds.
///
/// # Errors
///
/// Returns a gateway problem document when the identifier is malformed (400),
/// when the tenant owns no such plugin (404) and when an upstream or a route
/// still references it (409, `reason: PLUGIN_IN_USE`).
pub async fn delete_plugin(
    Extension(plane): Extension<Arc<ControlPlane>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    with_instance(uri.path(), async {
        let id = parse_plugin_id(&id)?;
        plane.delete_plugin(ctx.subject_tenant_id(), id)?;
        Ok(gateway_response(StatusCode::NO_CONTENT))
    })
    .await
}

/// The gear-relative prefix of every proxy URL, without the trailing alias.
const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// The `alias` and the `/`-prefixed path suffix of a proxy URL.
///
/// The suffix is cut out of the raw path so that its percent-encoding survives:
/// the `Path` extractor would decode it and the upstream would receive bytes
/// the client never sent.
fn split_proxy_path(path: &str) -> Option<(&str, String)> {
    let rest = path.strip_prefix(PROXY_PREFIX)?;
    Some(match rest.split_once('/') {
        Some((alias, suffix)) => (alias, format!("/{suffix}")),
        None => (rest, String::new()),
    })
}

/// `ANY /oagw/v1/proxy/{alias}` and `ANY /oagw/v1/proxy/{alias}/{*path_suffix}`
///
/// Forwards the request to the upstream the alias resolves to, as a streaming
/// pass-through. Everything that can reject the request happens inside
/// [`DataPlaneService::proxy`], so this handler only translates between the
/// transport and the domain.
///
/// # Errors
///
/// Returns a gateway problem document for every documented rejection (400, 404,
/// 413, 502, 503, 504).
pub async fn proxy(
    Extension(data_plane): Extension<Arc<DataPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    request: Request,
) -> ApiResult<Response> {
    let path = uri.path().to_owned();
    with_instance(&path, async {
        let (alias, path_suffix) = split_proxy_path(&path)
            .ok_or_else(|| OagwError::validation(format!("`{path}` is not an OAGW proxy URL")))?;
        let response = data_plane
            .proxy(request, ctx.subject_tenant_id(), alias, &path_suffix)
            .await?;
        // `with_error_source` never overrides the header, so the
        // `X-OAGW-Error-Source: upstream` stamp of the pass-through survives.
        Ok(gateway_response(response))
    })
    .await
}
