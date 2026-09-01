//! Plugin management handlers (`/api/oagw/v1/plugins`).
//!
//! Plugins are immutable: there is no `PUT` (DESIGN §3.3 "Plugins are
//! immutable (no PUT)"). `GET /plugins/{id}/source` returns the Starlark
//! source of a plugin.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, RawQuery};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::{Json, created_json, no_content};
use toolkit_security::context::SecurityContext;

use super::common;
use crate::api::rest::dto::{CreatePluginRequest, PluginDto, PluginSourceDto};
use crate::api::rest::query::ListQuery;
use crate::domain::error::DomainError;
use crate::domain::models::Plugin;
use crate::domain::service::{ControlPlaneService, PluginDraft};

/// Service extension type shared by every handler.
pub type Service = Arc<ControlPlaneService>;

/// `POST /plugins` — 201 with the created plugin.
pub async fn create_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
    axum::Json(request): axum::Json<CreatePluginRequest>,
) -> Result<Response, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let draft = PluginDraft {
        plugin_type: request.plugin_type,
        name: request.name,
        description: request.description,
        phases: request.phases,
        config_schema: request.config_schema,
        source_code: request.source_code,
    };
    let plugin = service.create_plugin(tenant_id, draft).await?;
    let id = common::resource_id(plugin.plugin_type.type_id(), plugin.id);
    Ok(created_json(PluginDto::from(plugin), &uri, &id).into_response())
}

/// `GET /plugins`
pub async fn list_plugins(
    RawQuery(query): RawQuery,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<Json<toolkit_odata::Page<serde_json::Value>>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let query = ListQuery::parse(query.as_deref())?;
    let items = service.list_plugins(tenant_id).await?;
    common::paged(&items, &query, |plugin| {
        serde_json::to_value(PluginDto::from(plugin.clone())).unwrap_or(serde_json::Value::Null)
    })
}

/// `GET /plugins/{id}`
pub async fn get_plugin(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<Json<PluginDto>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = parse_plugin_id(&raw_id)?;
    let plugin = service.get_plugin(tenant_id, id).await?;
    Ok(Json(PluginDto::from(plugin)))
}

/// `GET /plugins/{id}/source`
pub async fn get_plugin_source(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<Json<PluginSourceDto>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = parse_plugin_id(&raw_id)?;
    let (plugin, source) = service.get_plugin_source(tenant_id, id).await?;
    Ok(Json(PluginSourceDto {
        id: plugin.gts_id(),
        plugin_type: plugin.plugin_type,
        name: plugin.name,
        source_code: source,
    }))
}

/// `DELETE /plugins/{id}` — 409 when the plugin is still referenced.
pub async fn delete_plugin(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<impl IntoResponse, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = parse_plugin_id(&raw_id)?;
    service.delete_plugin(tenant_id, id).await?;
    Ok(no_content())
}

/// Plugin ids are typed by their family, so any `*_plugin` GTS id is accepted.
fn parse_plugin_id(raw: &str) -> Result<uuid::Uuid, DomainError> {
    if let Ok(id) = raw.trim().parse::<uuid::Uuid>() {
        return Ok(id);
    }
    for type_id in [
        crate::domain::models::AUTH_PLUGIN_TYPE,
        crate::domain::models::GUARD_PLUGIN_TYPE,
        crate::domain::models::TRANSFORM_PLUGIN_TYPE,
    ] {
        if let Some(suffix) = crate::domain::models::strip_gts_prefix(type_id, raw)
            && let Ok(id) = suffix.parse::<uuid::Uuid>()
        {
            return Ok(id);
        }
    }
    Err(DomainError::validation_with_value(
        "identifier must be a UUID or a 'gts.cf.core.oagw.{type}_plugin.v1~' GTS id",
        raw.to_owned(),
    ))
}

/// Serializes a plugin (list projection helper).
#[must_use]
pub fn plugin_json(plugin: &Plugin) -> serde_json::Value {
    serde_json::to_value(PluginDto::from(plugin.clone())).unwrap_or(serde_json::Value::Null)
}
