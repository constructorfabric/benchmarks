//! REST handlers for the OAGW management + proxy APIs.
//!
//! Management handlers follow the toolkit `ApiResult<CanonicalError>`
//! pattern; the proxy handler returns a raw [`axum::response::Response`]
//! because the data plane owns the full response lifecycle (including
//! streaming SSE bodies and WebSocket upgrades).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, Request};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use super::dto::{ListQuery, PluginsResponse, RoutesResponse, UpstreamsResponse};
use super::error::OagwError;
use crate::domain::data_plane::DataPlaneService;
use crate::domain::dto::{PluginKind, PluginRequest, RouteRequest, UpstreamRequest};
use crate::domain::error::{self, DomainError};
use crate::domain::service::ControlPlaneService;
use crate::infra::storage::InMemoryRepository;

/// Concrete control-plane service used by the REST layer.
pub type ControlPlane = ControlPlaneService<InMemoryRepository>;

/// Check a bearer scope, returning a canonical 403 when absent.
fn require_scope(ctx: &SecurityContext, perm: &str) -> Result<(), CanonicalError> {
    if error::scope_allows(ctx.token_scopes(), perm) {
        Ok(())
    } else {
        Err(OagwError::permission_denied()
            .with_reason(format!("missing required permission: {perm}"))
            .create())
    }
}

/// Permission identifier for a custom plugin of the given kind.
fn plugin_perm(kind: PluginKind, action: &str) -> &'static str {
    match kind {
        PluginKind::Auth => match action {
            "create" => error::PERM_AUTH_PLUGIN_CREATE,
            "read" => error::PERM_AUTH_PLUGIN_READ,
            _ => error::PERM_AUTH_PLUGIN_DELETE,
        },
        PluginKind::Guard => match action {
            "create" => error::PERM_GUARD_PLUGIN_CREATE,
            "read" => error::PERM_GUARD_PLUGIN_READ,
            _ => error::PERM_GUARD_PLUGIN_DELETE,
        },
        PluginKind::Transform => match action {
            "create" => error::PERM_TRANSFORM_PLUGIN_CREATE,
            "read" => error::PERM_TRANSFORM_PLUGIN_READ,
            _ => error::PERM_TRANSFORM_PLUGIN_DELETE,
        },
    }
}

// ---- upstreams -----------------------------------------------------------

/// `POST /api/oagw/v1/upstreams`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the upstream-create permission, or
/// with a control-plane `CanonicalError` (e.g. 409 alias conflict) when the
/// request is rejected by the service.
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Json(req): Json<UpstreamRequest>,
) -> ApiResult<(StatusCode, Json<crate::domain::dto::Upstream>)> {
    require_scope(&ctx, error::PERM_UPSTREAM_CREATE)?;
    let tenant = ctx.subject_tenant_id();
    let upstream = svc.create_upstream(&ctx, tenant, &req).await?;
    Ok((StatusCode::CREATED, Json(upstream)))
}

/// `GET /api/oagw/v1/upstreams`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the upstream-read permission.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<UpstreamsResponse>> {
    require_scope(&ctx, error::PERM_UPSTREAM_READ)?;
    let tenant = ctx.subject_tenant_id();
    let items = query.paginate(svc.list_upstreams(tenant));
    let count = items.len();
    Ok(Json(UpstreamsResponse { items, count }))
}

/// `GET /api/oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the upstream-read permission, or with
/// 404 when the upstream does not exist.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<crate::domain::dto::Upstream>> {
    require_scope(&ctx, error::PERM_UPSTREAM_READ)?;
    let tenant = ctx.subject_tenant_id();
    let upstream = svc.get_upstream_by_id(tenant, id)?;
    Ok(Json(upstream))
}

/// `PUT /api/oagw/v1/upstreams/{id}` — full replacement; the alias is
/// immutable, so the stored alias is preserved.
///
/// # Errors
///
/// Fails with 403 when the caller lacks the upstream-override permission, 404
/// when the upstream does not exist, or a control-plane `CanonicalError` when
/// the replacement is rejected by the service.
pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpstreamRequest>,
) -> ApiResult<Json<crate::domain::dto::Upstream>> {
    require_scope(&ctx, error::PERM_UPSTREAM_OVERRIDE)?;
    let tenant = ctx.subject_tenant_id();
    let existing = svc.get_upstream_by_id(tenant, id)?;
    let upstream = svc
        .replace_upstream(&ctx, tenant, &existing.alias, &req)
        .await?;
    Ok(Json(upstream))
}

/// `DELETE /api/oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the upstream-delete permission, or
/// with 404 when the upstream does not exist.
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_scope(&ctx, error::PERM_UPSTREAM_DELETE)?;
    let tenant = ctx.subject_tenant_id();
    let existing = svc.get_upstream_by_id(tenant, id)?;
    match svc.delete_upstream(tenant, &existing.alias)? {
        crate::domain::repo::DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        crate::domain::repo::DeleteOutcome::NotFound => {
            Err(DomainError::NotFound(format!("upstream '{id}' not found")).into())
        }
    }
}

// ---- routes --------------------------------------------------------------

/// `POST /api/oagw/v1/routes`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the route-create permission, or a
/// control-plane `CanonicalError` (e.g. 409 match-rule conflict) when the
/// request is rejected by the service.
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Json(req): Json<RouteRequest>,
) -> ApiResult<(StatusCode, Json<crate::domain::dto::Route>)> {
    require_scope(&ctx, error::PERM_ROUTE_CREATE)?;
    let tenant = ctx.subject_tenant_id();
    let route = svc.create_route(tenant, &req)?;
    Ok((StatusCode::CREATED, Json(route)))
}

/// `GET /api/oagw/v1/routes`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the route-read permission.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<RoutesResponse>> {
    require_scope(&ctx, error::PERM_ROUTE_READ)?;
    let tenant = ctx.subject_tenant_id();
    let items = query.paginate(svc.list_routes(tenant));
    let count = items.len();
    Ok(Json(RoutesResponse { items, count }))
}

/// `GET /api/oagw/v1/routes/{id}`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the route-read permission, or with
/// 404 when the route does not exist.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<crate::domain::dto::Route>> {
    require_scope(&ctx, error::PERM_ROUTE_READ)?;
    let tenant = ctx.subject_tenant_id();
    let route = svc.get_route(tenant, id)?;
    Ok(Json(route))
}

/// `PUT /api/oagw/v1/routes/{id}`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the route-override permission, 404
/// when the route does not exist, or a control-plane `CanonicalError` when the
/// replacement is rejected by the service.
pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
    Json(req): Json<RouteRequest>,
) -> ApiResult<Json<crate::domain::dto::Route>> {
    require_scope(&ctx, error::PERM_ROUTE_OVERRIDE)?;
    let tenant = ctx.subject_tenant_id();
    let route = svc.replace_route(tenant, id, &req)?;
    Ok(Json(route))
}

/// `DELETE /api/oagw/v1/routes/{id}`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the route-delete permission, or with
/// 404 when the route does not exist.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_scope(&ctx, error::PERM_ROUTE_DELETE)?;
    let tenant = ctx.subject_tenant_id();
    match svc.delete_route(tenant, id)? {
        crate::domain::repo::DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        crate::domain::repo::DeleteOutcome::NotFound => {
            Err(DomainError::NotFound(format!("route '{id}' not found")).into())
        }
    }
}

// ---- custom plugins ------------------------------------------------------

/// `POST /api/oagw/v1/plugins`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the matching plugin-kind create
/// permission, or a control-plane `CanonicalError` when the plugin is rejected
/// by the service.
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Json(req): Json<PluginRequest>,
) -> ApiResult<(StatusCode, Json<crate::domain::dto::CustomPlugin>)> {
    require_scope(&ctx, plugin_perm(req.plugin_type, "create"))?;
    let tenant = ctx.subject_tenant_id();
    let plugin = svc.create_plugin(tenant, &req)?;
    Ok((StatusCode::CREATED, Json(plugin)))
}

/// `GET /api/oagw/v1/plugins`
///
/// # Errors
///
/// Fails with 403 when the caller lacks at least one plugin-kind read
/// permission.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<PluginsResponse>> {
    // A mixed-kind list: allow when the caller may read at least one kind.
    let allowed = [
        error::PERM_AUTH_PLUGIN_READ,
        error::PERM_GUARD_PLUGIN_READ,
        error::PERM_TRANSFORM_PLUGIN_READ,
    ]
    .iter()
    .any(|p| error::scope_allows(ctx.token_scopes(), p));
    if !allowed {
        return Err(OagwError::permission_denied()
            .with_reason("missing required permission: at least one plugin read permission")
            .create());
    }
    let tenant = ctx.subject_tenant_id();
    let items = query.paginate(svc.list_plugins(tenant));
    let count = items.len();
    Ok(Json(PluginsResponse { items, count }))
}

/// `GET /api/oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Fails with 403 when the caller lacks the matching plugin-kind read
/// permission, or with 404 when the plugin does not exist.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<crate::domain::dto::CustomPlugin>> {
    let tenant = ctx.subject_tenant_id();
    let plugin = svc.get_plugin(tenant, id)?;
    require_scope(&ctx, plugin_perm(plugin.plugin_type, "read"))?;
    Ok(Json(plugin))
}

/// `DELETE /api/oagw/v1/plugins/{id}` — 409 `PluginInUse` when referenced.
///
/// Returns a raw [`axum::response::Response`] because the plugin-in-use
/// conflict must carry the exact `` `cf.oagw.plugin.in_use.v1` `` problem
/// `type` (ADR-0001:207), which the toolkit canonical `AlreadyExists` mapping
/// cannot express.
///
/// # Errors
///
/// Fails with 403 when the caller lacks the matching plugin-kind delete
/// permission, or with 404 when the plugin does not exist.
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let tenant = ctx.subject_tenant_id();
    let plugin = svc.get_plugin(tenant, id)?;
    require_scope(&ctx, plugin_perm(plugin.plugin_type, "delete"))?;
    match svc.delete_plugin(tenant, id) {
        Ok(crate::domain::repo::DeleteOutcome::Deleted) => {
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Ok(crate::domain::repo::DeleteOutcome::NotFound) => {
            Err(DomainError::NotFound(format!("plugin '{id}' not found")).into())
        }
        Err(DomainError::PluginInUse {
            plugin_id,
            upstreams,
            routes,
        }) => Ok(super::error::plugin_in_use_problem(
            &plugin_id, &upstreams, &routes,
        )),
        Err(e) => Err(e.into()),
    }
}

/// `GET /api/oagw/v1/plugins/{id}/source` — raw Starlark source.
///
/// # Errors
///
/// Fails with 403 when the caller lacks the matching plugin-kind read
/// permission, or with 404 when the plugin does not exist.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> ApiResult<String> {
    let tenant = ctx.subject_tenant_id();
    let plugin = svc.get_plugin(tenant, id)?;
    require_scope(&ctx, plugin_perm(plugin.plugin_type, "read"))?;
    Ok(plugin.source_code)
}

// ---- proxy ---------------------------------------------------------------

/// `{METHOD} /api/oagw/v1/proxy/{alias}` — bare alias (empty path suffix).
pub async fn proxy_bare(
    Extension(service): Extension<Arc<DataPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(alias): Path<String>,
    req: Request,
) -> Response {
    service.proxy(&ctx, req, &alias, "").await
}

/// `{METHOD} /api/oagw/v1/proxy/{alias}/{*suffix}`.
pub async fn proxy_suffixed(
    Extension(service): Extension<Arc<DataPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((alias, suffix)): Path<(String, String)>,
    req: Request,
) -> Response {
    service.proxy(&ctx, req, &alias, &suffix).await
}
