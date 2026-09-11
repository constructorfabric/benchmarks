//! REST handlers for upstream, route and plugin management.

use axum::Extension;
use axum::Json;
use axum::http::StatusCode;
use std::sync::Arc;
use toolkit_security::SecurityContext;

use crate::api::dto::{PluginDto, PluginSourceDto, RouteDto, UpstreamDto};
use crate::api::error::{ApiContext, instance_of};
use crate::domain::error::{DomainError, Problem};

/// `POST /oagw/v1/upstreams`
///
/// # Errors
/// Surfaces a problem document on validation failure or alias conflict.
pub async fn create_upstream(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    Json(upstream): Json<UpstreamDto>,
) -> Result<(StatusCode, Json<UpstreamDto>), Problem> {
    let control = Arc::clone(&ctx.control_plane);
    let tenant = tenant_of(&security);
    let user_alias = non_empty(upstream.alias.clone());
    let model = upstream.into_model()?;
    let created = control
        .create_upstream(tenant, user_alias, model)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok((StatusCode::CREATED, Json(UpstreamDto::from_model(&created))))
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
/// Surfaces a problem document when the store is unreachable.
pub async fn list_upstreams(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
) -> Result<Json<Vec<UpstreamDto>>, Problem> {
    let items = ctx
        .control_plane
        .list_upstreams(tenant_of(&security))
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(items.iter().map(UpstreamDto::from_model).collect()))
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
/// Returns 404 for an unknown identifier.
pub async fn get_upstream(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<UpstreamDto>, Problem> {
    let uuid = crate::api::gts_id::parse_upstream_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    let upstream = ctx
        .control_plane
        .get_upstream(tenant_of(&security), uuid)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(UpstreamDto::from_model(&upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// # Errors
/// Surfaces a problem document on validation failure or alias change.
pub async fn replace_upstream(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(upstream): Json<UpstreamDto>,
) -> Result<Json<UpstreamDto>, Problem> {
    let uuid = crate::api::gts_id::parse_upstream_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    let tenant = tenant_of(&security);
    let user_alias = non_empty(upstream.alias.clone());
    let model = upstream.into_model()?;
    let replaced = ctx
        .control_plane
        .replace_upstream(tenant, uuid, user_alias, model)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(UpstreamDto::from_model(&replaced)))
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
/// Returns 404 for an unknown identifier.
pub async fn delete_upstream(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, Problem> {
    let uuid = crate::api::gts_id::parse_upstream_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    ctx.control_plane
        .delete_upstream(tenant_of(&security), uuid)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/routes`
///
/// # Errors
/// Surfaces a problem document on validation failure or duplicate match.
pub async fn create_route(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    Json(route): Json<RouteDto>,
) -> Result<(StatusCode, Json<RouteDto>), Problem> {
    let tenant = tenant_of(&security);
    let model = route.into_model(tenant)?;
    let created = ctx
        .control_plane
        .create_route(tenant, model)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok((StatusCode::CREATED, Json(RouteDto::from_model(&created))))
}

/// `GET /oagw/v1/routes`
///
/// # Errors
/// Surfaces a problem document when the store fails.
pub async fn list_routes(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
) -> Result<Json<Vec<RouteDto>>, Problem> {
    let items = ctx
        .control_plane
        .list_routes(tenant_of(&security))
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(items.iter().map(RouteDto::from_model).collect()))
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
/// Returns 404 for an unknown identifier.
pub async fn get_route(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<RouteDto>, Problem> {
    let uuid = crate::api::gts_id::parse_route_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    let route = ctx
        .control_plane
        .get_route(tenant_of(&security), uuid)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(RouteDto::from_model(&route)))
}

/// `PUT /oagw/v1/routes/{id}`
///
/// # Errors
/// Surfaces a problem document on validation failure.
pub async fn replace_route(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(route): Json<RouteDto>,
) -> Result<Json<RouteDto>, Problem> {
    let uuid = crate::api::gts_id::parse_route_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    let tenant = tenant_of(&security);
    let model = route.into_model(tenant)?;
    let replaced = ctx
        .control_plane
        .replace_route(tenant, uuid, model)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(RouteDto::from_model(&replaced)))
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
/// Returns 404 for an unknown identifier.
pub async fn delete_route(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, Problem> {
    let uuid = crate::api::gts_id::parse_route_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    ctx.control_plane
        .delete_route(tenant_of(&security), uuid)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/plugins`
///
/// # Errors
/// Surfaces a problem document on validation failure.
pub async fn create_plugin(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    Json(plugin): Json<PluginDto>,
) -> Result<(StatusCode, Json<PluginDto>), Problem> {
    let tenant = tenant_of(&security);
    let model = plugin.into_model(tenant)?;
    let created = ctx
        .control_plane
        .create_plugin(tenant, model)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok((
        StatusCode::CREATED,
        Json(PluginDto::from_model(&created, tenant)),
    ))
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
/// Surfaces a problem document when the store fails.
pub async fn list_plugins(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
) -> Result<Json<Vec<PluginDto>>, Problem> {
    let tenant = tenant_of(&security);
    let items = ctx
        .control_plane
        .list_plugins(tenant)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(
        items
            .iter()
            .map(|plugin| PluginDto::from_model(plugin, tenant))
            .collect(),
    ))
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
/// Returns 404 for an unknown identifier.
pub async fn get_plugin(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<PluginDto>, Problem> {
    let tenant = tenant_of(&security);
    let uuid = crate::api::gts_id::parse_plugin_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    let plugin = ctx
        .control_plane
        .get_plugin(tenant, uuid)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(Json(PluginDto::from_model(&plugin, tenant)))
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
/// Returns 404 for an unknown identifier and 400 when no source is stored.
pub async fn get_plugin_source(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<PluginSourceDto>, Problem> {
    let tenant = tenant_of(&security);
    let uuid = crate::api::gts_id::parse_plugin_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    let plugin = ctx
        .control_plane
        .get_plugin(tenant, uuid)
        .await
        .map_err(|err| instance_of(&err))?;
    if plugin.source_code.is_none() {
        return Err(instance_of(&DomainError::validation(
            "the plugin carries no source code",
        )));
    }
    Ok(Json(PluginSourceDto::from_plugin(&plugin, id)))
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
/// Returns 404 for an unknown identifier and 409 while referenced.
pub async fn delete_plugin(
    Extension(ctx): Extension<Arc<crate::api::error::ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, Problem> {
    let uuid = crate::api::gts_id::parse_plugin_id(&id)
        .map_err(|err| instance_of(&DomainError::validation(err.to_string())))?;
    ctx.control_plane
        .delete_plugin(tenant_of(&security), uuid)
        .await
        .map_err(|err| instance_of(&err))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The tenant whose resources this request operates on.
#[must_use]
pub fn tenant_of(security: &SecurityContext) -> uuid::Uuid {
    security.subject_tenant_id()
}

/// An explicitly provided alias, or `None` when the client left it empty.
#[must_use]
fn non_empty(alias: String) -> Option<String> {
    if alias.trim().is_empty() {
        None
    } else {
        Some(alias)
    }
}

/// `GET /oagw/v1/config` — the effective data-plane configuration.
///
/// # Errors
/// Never fails; the configuration is held in memory.
pub async fn get_config(
    Extension(ctx): Extension<Arc<ApiContext>>,
) -> Result<Json<ConfigDto>, Problem> {
    Ok(Json(ConfigDto {
        allow_http_upstream: ctx.config.allow_http_upstream,
        max_request_body_bytes: ctx.config.max_request_body_bytes,
        proxy_timeout_secs: ctx.config.proxy_timeout_secs,
    }))
}

/// Effective data-plane configuration.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigDto {
    /// Whether plaintext upstreams are accepted.
    pub allow_http_upstream: bool,
    /// Hard request-body ceiling in bytes.
    pub max_request_body_bytes: u64,
    /// Upstream call timeout in seconds.
    pub proxy_timeout_secs: u64,
}
