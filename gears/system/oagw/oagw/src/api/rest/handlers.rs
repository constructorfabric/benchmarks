//! REST handlers for the OAGW Control-Plane Management API (feature
//! `cpt-cf-oagw-feature-control-plane-api`, flows
//! `cpt-cf-oagw-flow-control-plane-api-{upstream,route,plugin}-crud`).
//!
//! Every handler takes the subject [`SecurityContext`] and the shared
//! [`GearState`] via axum `Extension`s, delegates to the
//! [`ControlPlaneService`], and renders domain failures as the OAGW RFC 9457
//! [`GatewayError`] envelope carrying `X-OAGW-Error-Source: gateway` (DoD
//! `cpt-cf-oagw-dod-error-semantics-envelope`; algorithm
//! `cpt-cf-oagw-algo-error-semantics-build-instance`).  Request correlation
//! honors DoD `cpt-cf-oagw-dod-error-semantics-request-id` (`X-Request-ID` /
//! `traceparent`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::http::header::LOCATION;
use axum::http::{HeaderMap, Uri};
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::GearState;
use crate::domain::error::DomainError;
use crate::infra::audit::{config_change, now_utc_timestamp};
use crate::infra::error_envelope::{ErrorRequestContext, GatewayError};

use super::dto::{
    EqFilter, ListParams, OrderBy, PluginCreateDto, PluginViewDto, RouteRequestDto, RouteViewDto,
    UpstreamRequestDto, UpstreamViewDto, plugin_source_dto,
};

/// Builds the RFC 9457 request context from the request URI and headers
/// (`request_path` = the request path, `request_id` = `X-Request-ID`,
/// `trace_id` = the `traceparent` header; algorithm
/// `cpt-cf-oagw-algo-error-semantics-propagate-request-id`).
fn request_ctx(uri: &Uri, headers: &HeaderMap) -> ErrorRequestContext {
    ErrorRequestContext {
        request_path: uri.path().to_owned(),
        request_id: headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        trace_id: headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    }
}

/// Renders a domain error as an OAGW gateway-error envelope against `ctx`.
fn render(err: DomainError, ctx: &ErrorRequestContext) -> GatewayError {
    GatewayError::from_domain(&err, ctx)
}

/// Emits one structured config-change audit entry (DESIGN §4.3 "Config
/// changes": upstream/route/plugin create/update/delete) after a successful
/// management mutation (feature `cpt-cf-oagw-feature-observability-audit`,
/// DoD `cpt-cf-oagw-dod-observability-audit-audit-log`).
fn audit_config_change(
    ctx: &SecurityContext,
    ectx: &ErrorRequestContext,
    event: &str,
    method: &str,
    path: &str,
    status: u16,
) {
    config_change(
        now_utc_timestamp(),
        opt_uuid(ctx.subject_tenant_id()),
        opt_uuid(ctx.subject_id()),
        ectx.request_id.clone(),
        event,
        method,
        path,
        status,
    )
    .emit();
}

/// `Some(uuid_string)` for a non-nil id (anonymous contexts are nil and
/// recorded as absent).
fn opt_uuid(id: Uuid) -> Option<String> {
    if id.is_nil() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Applies the OData-style list parameters to a tenant-scoped collection
/// (algorithm `cpt-cf-oagw-algo-control-plane-api-odata`):
///
/// - `$filter` — equality-only (`field eq|ne value`), unsupported expressions
///   surface as a 400;
/// - `$orderby` — `field [asc|desc]` on the documented keys, a 400 for keys
///   no item exposes;
/// - `$skip` then `$top` (default [`ListParams::DEFAULT_TOP`], capped at
///   [`ListParams::MAX_TOP`]).
///
/// # Errors
/// [`DomainError::Validation`] (400) on an unsupported `$filter`/`$orderby`
/// expression or an unknown sort key.
fn apply_list_params<T>(
    items: &mut Vec<T>,
    params: &ListParams,
    key: impl Fn(&T, &str) -> Option<String>,
) -> Result<(), DomainError> {
    if let Some(raw) = &params.filter {
        let f = EqFilter::parse(raw)?;
        let is_eq = f.is_eq;
        items.retain(|item| match key(item, &f.field) {
            Some(v) => {
                let equal = v == f.value;
                if is_eq { equal } else { !equal }
            }
            None => !is_eq,
        });
    }
    if let Some(raw) = &params.orderby {
        let ob = OrderBy::parse(raw)?;
        if items.iter().all(|item| key(item, &ob.field).is_none()) {
            return Err(DomainError::validation(
                Some("$orderby"),
                format!("unsupported $orderby field: '{}'", ob.field),
            ));
        }
        let mut sortable: Vec<(String, T)> = items
            .drain(..)
            .filter_map(|item| key(&item, &ob.field).map(|k| (k, item)))
            .collect();
        sortable.sort_by(|a, b| a.0.cmp(&b.0));
        if ob.desc {
            sortable.reverse();
        }
        *items = sortable.into_iter().map(|(_, item)| item).collect();
    }
    let start = params.skip().min(items.len());
    items.drain(..start);
    items.truncate(params.top());
    Ok(())
}

/// Sort/filter keys exposed by the upstream list representation.
fn upstream_key(item: &UpstreamViewDto, field: &str) -> Option<String> {
    match field {
        "id" => Some(item.id.to_string()),
        "alias" => Some(item.alias.clone()),
        "enabled" => Some(item.enabled.to_string()),
        _ => None,
    }
}

/// Sort/filter keys exposed by the route list representation.
fn route_key(item: &RouteViewDto, field: &str) -> Option<String> {
    match field {
        "id" => Some(item.id.to_string()),
        "upstream_id" => Some(item.upstream_id.to_string()),
        "priority" => Some(item.priority.to_string()),
        "enabled" => Some(item.enabled.to_string()),
        "path" => match &item.match_ {
            super::dto::MatchDto::Http(m) => Some(m.path.clone()),
            super::dto::MatchDto::Grpc(_) => None,
        },
        _ => None,
    }
}

/// Sort/filter keys exposed by the plugin list representation.
fn plugin_key(item: &PluginViewDto, field: &str) -> Option<String> {
    match field {
        "id" => Some(item.id.to_string()),
        "name" => Some(item.name.clone()),
        "plugin_type" => Some(format!("{:?}", item.plugin_type).to_lowercase()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Upstream handlers (flow `cpt-cf-oagw-flow-control-plane-api-upstream-crud`)
// ---------------------------------------------------------------------------

/// `POST /upstreams`
///
/// # Errors
/// 400/403/409/503 as OAGW gateway-error envelopes.
pub async fn create_upstream(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Json(body): Json<UpstreamRequestDto>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let draft = body.into_draft().map_err(|e| render(e, &ectx))?;
    let created = state
        .control
        .create_upstream(&ctx, draft)
        .await
        .map_err(|e| render(e, &ectx))?;
    let dto = UpstreamViewDto::from(&created);
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), created.id);
    audit_config_change(
        &ctx,
        &ectx,
        "config.upstream.created",
        "POST",
        uri.path(),
        201,
    );
    Ok((StatusCode::CREATED, [(LOCATION, location)], Json(dto)).into_response())
}

/// `PUT /upstreams/{id}` — replace an existing upstream (alias immutable).
///
/// # Errors
/// 400/403/404/409/503 as OAGW gateway-error envelopes.
pub async fn replace_upstream(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpstreamRequestDto>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let draft = body.into_draft().map_err(|e| render(e, &ectx))?;
    match state
        .control
        .replace_upstream(&ctx, id, draft)
        .await
        .map_err(|e| render(e, &ectx))?
    {
        Some(upstream) => {
            let dto = UpstreamViewDto::from(&upstream);
            audit_config_change(
                &ctx,
                &ectx,
                "config.upstream.replaced",
                "PUT",
                uri.path(),
                200,
            );
            Ok((StatusCode::OK, Json(dto)).into_response())
        }
        None => Err(render(
            DomainError::RouteNotFound {
                detail: format!("upstream '{id}' not found in this tenant"),
            },
            &ectx,
        )),
    }
}

/// `GET /upstreams` — list with OData-style params.
///
/// # Errors
/// 400/403/503 as OAGW gateway-error envelopes.
pub async fn list_upstreams(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let mut items: Vec<UpstreamViewDto> = state
        .control
        .list_upstreams(&ctx)
        .await
        .map_err(|e| render(e, &ectx))?
        .iter()
        .map(UpstreamViewDto::from)
        .collect();
    apply_list_params(&mut items, &params, upstream_key).map_err(|e| render(e, &ectx))?;
    Ok((StatusCode::OK, Json(items)).into_response())
}

/// `GET /upstreams/{id}`
///
/// # Errors
/// 403/404/503 as OAGW gateway-error envelopes.
pub async fn get_upstream(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    match state
        .control
        .get_upstream(&ctx, id)
        .await
        .map_err(|e| render(e, &ectx))?
    {
        Some(upstream) => {
            let dto = UpstreamViewDto::from(&upstream);
            Ok((StatusCode::OK, Json(dto)).into_response())
        }
        None => Err(render(
            DomainError::RouteNotFound {
                detail: format!("upstream '{id}' not found in this tenant"),
            },
            &ectx,
        )),
    }
}

/// `DELETE /upstreams/{id}`
///
/// # Errors
/// 403/404/503 as OAGW gateway-error envelopes.
pub async fn delete_upstream(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let deleted = state
        .control
        .delete_upstream(&ctx, id)
        .await
        .map_err(|e| render(e, &ectx))?;
    if deleted {
        audit_config_change(
            &ctx,
            &ectx,
            "config.upstream.deleted",
            "DELETE",
            uri.path(),
            204,
        );
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(render(
            DomainError::RouteNotFound {
                detail: format!("upstream '{id}' not found in this tenant"),
            },
            &ectx,
        ))
    }
}

// ---------------------------------------------------------------------------
// Route handlers (flow `cpt-cf-oagw-flow-control-plane-api-route-crud`)
// ---------------------------------------------------------------------------

/// `POST /routes`
///
/// # Errors
/// 400/403/404/409/503 as OAGW gateway-error envelopes.
pub async fn create_route(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Json(body): Json<RouteRequestDto>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let draft = body.into_draft().map_err(|e| render(e, &ectx))?;
    let created = state
        .control
        .create_route(&ctx, draft)
        .await
        .map_err(|e| render(e, &ectx))?;
    let dto = RouteViewDto::from(&created);
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), created.id);
    audit_config_change(&ctx, &ectx, "config.route.created", "POST", uri.path(), 201);
    Ok((StatusCode::CREATED, [(LOCATION, location)], Json(dto)).into_response())
}

/// `PUT /routes/{id}` — replace an existing route (`upstream_id` immutable).
///
/// # Errors
/// 400/403/404/409/503 as OAGW gateway-error envelopes.
pub async fn replace_route(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
    Json(body): Json<RouteRequestDto>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let draft = body.into_draft().map_err(|e| render(e, &ectx))?;
    match state
        .control
        .replace_route(&ctx, id, draft)
        .await
        .map_err(|e| render(e, &ectx))?
    {
        Some(route) => {
            let dto = RouteViewDto::from(&route);
            audit_config_change(&ctx, &ectx, "config.route.replaced", "PUT", uri.path(), 200);
            Ok((StatusCode::OK, Json(dto)).into_response())
        }
        None => Err(render(
            DomainError::RouteNotFound {
                detail: format!("route '{id}' not found in this tenant"),
            },
            &ectx,
        )),
    }
}

/// `GET /routes` — list with OData-style params.
///
/// # Errors
/// 400/403/503 as OAGW gateway-error envelopes.
pub async fn list_routes(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let mut items: Vec<RouteViewDto> = state
        .control
        .list_routes(&ctx)
        .await
        .map_err(|e| render(e, &ectx))?
        .iter()
        .map(RouteViewDto::from)
        .collect();
    apply_list_params(&mut items, &params, route_key).map_err(|e| render(e, &ectx))?;
    Ok((StatusCode::OK, Json(items)).into_response())
}

/// `GET /routes/{id}`
///
/// # Errors
/// 403/404/503 as OAGW gateway-error envelopes.
pub async fn get_route(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    match state
        .control
        .get_route(&ctx, id)
        .await
        .map_err(|e| render(e, &ectx))?
    {
        Some(route) => {
            let dto = RouteViewDto::from(&route);
            Ok((StatusCode::OK, Json(dto)).into_response())
        }
        None => Err(render(
            DomainError::RouteNotFound {
                detail: format!("route '{id}' not found in this tenant"),
            },
            &ectx,
        )),
    }
}

/// `DELETE /routes/{id}`
///
/// # Errors
/// 403/404/503 as OAGW gateway-error envelopes.
pub async fn delete_route(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let deleted = state
        .control
        .delete_route(&ctx, id)
        .await
        .map_err(|e| render(e, &ectx))?;
    if deleted {
        audit_config_change(
            &ctx,
            &ectx,
            "config.route.deleted",
            "DELETE",
            uri.path(),
            204,
        );
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(render(
            DomainError::RouteNotFound {
                detail: format!("route '{id}' not found in this tenant"),
            },
            &ectx,
        ))
    }
}

// ---------------------------------------------------------------------------
// Plugin handlers (flow `cpt-cf-oagw-flow-control-plane-api-plugin-crud`)
// ---------------------------------------------------------------------------

/// `POST /plugins`
///
/// # Errors
/// 400/403/409/503 as OAGW gateway-error envelopes.
pub async fn create_plugin(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Json(body): Json<PluginCreateDto>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    if body.name.trim().is_empty() {
        return Err(render(
            DomainError::validation(Some("name"), "plugin name must not be empty"),
            &ectx,
        ));
    }
    let created = state
        .control
        .create_plugin(
            &ctx,
            body.plugin_type.into_entity(),
            body.name,
            body.config_schema,
            body.source_code,
        )
        .await
        .map_err(|e| render(e, &ectx))?;
    let dto = PluginViewDto::from(&created);
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), created.id);
    audit_config_change(
        &ctx,
        &ectx,
        "config.plugin.created",
        "POST",
        uri.path(),
        201,
    );
    Ok((StatusCode::CREATED, [(LOCATION, location)], Json(dto)).into_response())
}

/// `GET /plugins` — list with OData-style params.
///
/// # Errors
/// 400/403/503 as OAGW gateway-error envelopes.
pub async fn list_plugins(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let mut items: Vec<PluginViewDto> = state
        .control
        .list_plugins(&ctx)
        .await
        .map_err(|e| render(e, &ectx))?
        .iter()
        .map(PluginViewDto::from)
        .collect();
    apply_list_params(&mut items, &params, plugin_key).map_err(|e| render(e, &ectx))?;
    Ok((StatusCode::OK, Json(items)).into_response())
}

/// `GET /plugins/{id}`
///
/// # Errors
/// 403/404/503 as OAGW gateway-error envelopes.
pub async fn get_plugin(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    match state
        .control
        .get_plugin(&ctx, id)
        .await
        .map_err(|e| render(e, &ectx))?
    {
        Some(plugin) => {
            let dto = PluginViewDto::from(&plugin);
            Ok((StatusCode::OK, Json(dto)).into_response())
        }
        None => Err(render(
            DomainError::RouteNotFound {
                detail: format!("plugin '{id}' not found in this tenant"),
            },
            &ectx,
        )),
    }
}

/// `GET /plugins/{id}/source`
///
/// # Errors
/// 403/404/503 as OAGW gateway-error envelopes.
pub async fn get_plugin_source(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    match state
        .control
        .plugin_source(&ctx, id)
        .await
        .map_err(|e| render(e, &ectx))?
    {
        Some(source) => {
            let dto = plugin_source_dto(source);
            Ok((StatusCode::OK, Json(dto)).into_response())
        }
        None => Err(render(
            DomainError::RouteNotFound {
                detail: format!("plugin '{id}' not found in this tenant"),
            },
            &ectx,
        )),
    }
}

/// `DELETE /plugins/{id}` — refuses (409 `plugin.in_use`) while still bound.
///
/// # Errors
/// 403/404/409/503 as OAGW gateway-error envelopes.
pub async fn delete_plugin(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, GatewayError> {
    let ectx = request_ctx(&uri, &headers);
    let deleted = state
        .control
        .delete_plugin(&ctx, id)
        .await
        .map_err(|e| render(e, &ectx))?;
    if deleted {
        audit_config_change(
            &ctx,
            &ectx,
            "config.plugin.deleted",
            "DELETE",
            uri.path(),
            204,
        );
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(render(
            DomainError::RouteNotFound {
                detail: format!("plugin '{id}' not found in this tenant"),
            },
            &ectx,
        ))
    }
}
