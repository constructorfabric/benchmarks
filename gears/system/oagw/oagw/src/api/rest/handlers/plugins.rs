// @cpt-begin:cpt-cf-oagw-dod-plugin-api-create:p2:inst-plugin-handlers
//! Plugin catalogue handlers.
//!
//! Plugins are immutable after creation, so there is deliberately no replace
//! endpoint. Custom plugin source is stored verbatim and never executed in this
//! build, because no sandbox runtime is enabled.

use crate::api::rest::dto::{ListQuery, PluginCreateDto, PluginDto, PluginListDto};
use crate::api::rest::error::set_error_source;
use crate::api::rest::handlers::upstreams::{parse_id, validate_list_query};
use crate::api::rest::state::OagwState;
use crate::domain::error::{DomainError, ErrorKind, ErrorSource};
use crate::domain::model::{Plugin, PluginType, gts_resource_id};
use crate::infra::store::PluginDeleteError;
use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn plugin_kind_segment(plugin_type: PluginType) -> &'static str {
    match plugin_type {
        PluginType::Auth => "auth_plugin",
        PluginType::Guard => "guard_plugin",
        PluginType::Transform => "transform_plugin",
    }
}

fn to_dto(plugin: Plugin) -> PluginDto {
    PluginDto {
        id: gts_resource_id(plugin_kind_segment(plugin.plugin_type), plugin.id),
        uuid: plugin.id,
        plugin_type: plugin.plugin_type,
        name: plugin.name,
        description: plugin.description,
        config_schema: plugin.config_schema,
        phases: plugin.phases,
        source_code: plugin.source_code,
    }
}

/// Validate that `config_schema` is a syntactically plausible JSON Schema
/// object.
///
/// This is a modest, dependency-free check: it confirms the document is
/// either absent (`null`) or an object, and that two commonly-misused members
/// have the shape JSON Schema requires when they are present. Full schema
/// compilation (draft resolution, `$ref` handling, keyword validation, and so
/// on) is out of scope for this build; a malformed schema that passes here can
/// still be rejected by a future, stricter validator without a contract
/// change.
///
/// # Errors
/// Returns a validation error when `config_schema` is not `null` or an
/// object, when its `type` member is present but is neither a string nor an
/// array of strings, or when its `properties` member is present but is not an
/// object.
fn validate_config_schema(schema: &serde_json::Value) -> Result<(), DomainError> {
    if schema.is_null() {
        return Ok(());
    }
    let Some(object) = schema.as_object() else {
        return Err(DomainError::validation(
            "config_schema must be a JSON Schema object",
        ));
    };
    if let Some(type_member) = object.get("type") {
        let is_string_or_string_array = type_member.is_string()
            || type_member
                .as_array()
                .is_some_and(|items| items.iter().all(serde_json::Value::is_string));
        if !is_string_or_string_array {
            return Err(DomainError::validation(
                "config_schema.type must be a string or an array of strings",
            ));
        }
    }
    if let Some(properties) = object.get("properties")
        && !properties.is_object()
    {
        return Err(DomainError::validation(
            "config_schema.properties must be an object",
        ));
    }
    Ok(())
}

fn json_ok<T: serde::Serialize>(status: StatusCode, body: &T) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    set_error_source(&mut response, ErrorSource::Gateway);
    response
}

/// Create a custom plugin.
///
/// # Errors
/// Returns a validation error on a malformed body and a conflict when the name
/// is already taken within the tenant.
pub async fn create(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    axum::Json(body): axum::Json<PluginCreateDto>,
) -> Result<Response, DomainError> {
    if body.name.trim().is_empty() {
        return Err(DomainError::validation("name must not be empty"));
    }
    if body.source_code.trim().is_empty() {
        return Err(DomainError::validation("source_code must not be empty"));
    }
    if body.plugin_type == PluginType::Transform {
        if body.phases.is_empty() {
            return Err(DomainError::validation(
                "phases must not be empty for a transform plugin",
            ));
        }
    } else if !body.phases.is_empty() {
        return Err(DomainError::validation(
            "phases may only be supplied for a transform plugin",
        ));
    }
    validate_config_schema(&body.config_schema)?;
    let plugin = Plugin {
        id: Uuid::new_v4(),
        tenant_id: ctx.subject_tenant_id(),
        plugin_type: body.plugin_type,
        name: body.name,
        description: body.description,
        config_schema: body.config_schema,
        phases: body.phases,
        source_code: body.source_code,
    };
    let created = state.store.create_plugin(plugin)?;
    Ok(json_ok(StatusCode::CREATED, &to_dto(created)))
}

/// List the tenant's plugins.
///
/// # Errors
/// Returns a validation error when a query parameter is malformed.
pub async fn list(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, DomainError> {
    validate_list_query(&query)?;
    let mut items: Vec<PluginDto> = state
        .store
        .list_plugins(ctx.subject_tenant_id())
        .into_iter()
        .map(to_dto)
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    let page: Vec<PluginDto> = items
        .into_iter()
        .skip(query.effective_skip())
        .take(query.effective_top())
        .collect();
    let count = page.len();
    Ok(json_ok(
        StatusCode::OK,
        &PluginListDto { items: page, count },
    ))
}

/// Fetch one plugin, including its stored source.
///
/// # Errors
/// Returns not-found when the tenant does not own a plugin with that id.
pub async fn get(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    let found = state
        .store
        .get_plugin(ctx.subject_tenant_id(), id)
        .ok_or_else(|| DomainError::not_found("plugin not found"))?;
    Ok(json_ok(StatusCode::OK, &to_dto(found)))
}

/// Fetch a plugin's source verbatim.
///
/// # Errors
/// Returns not-found when the tenant does not own a plugin with that id.
pub async fn get_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    let found = state
        .store
        .get_plugin(ctx.subject_tenant_id(), id)
        .ok_or_else(|| DomainError::not_found("plugin not found"))?;
    let mut response = (
        StatusCode::OK,
        [(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        found.source_code,
    )
        .into_response();
    set_error_source(&mut response, ErrorSource::Gateway);
    Ok(response)
}

/// Delete a plugin that nothing references.
///
/// # Errors
/// Returns not-found when the plugin does not exist for the tenant, and a
/// conflict naming every referring upstream and route when it is still bound.
pub async fn delete(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    let tenant_id = ctx.subject_tenant_id();
    // A single store call performs the existence check, the reference scan
    // and the removal under one write guard, so a concurrent request cannot
    // bind the plugin between the scan and the delete.
    match state.store.delete_plugin_checked(tenant_id, id) {
        Ok(()) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            set_error_source(&mut response, ErrorSource::Gateway);
            Ok(response)
        }
        Err(PluginDeleteError::NotFound) => Err(DomainError::not_found("plugin not found")),
        Err(PluginDeleteError::StillReferenced(references)) => {
            let upstreams: Vec<String> = references.upstreams.iter().map(Uuid::to_string).collect();
            let routes: Vec<String> = references.routes.iter().map(Uuid::to_string).collect();
            Err(DomainError::new(
                ErrorKind::PluginInUse,
                "plugin is still referenced and cannot be deleted",
            )
            .with_context(serde_json::json!({
                "plugin_id": id.to_string(),
                "referenced_by": { "upstreams": upstreams, "routes": routes },
            })))
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-plugin-api-create:p2:inst-plugin-handlers
