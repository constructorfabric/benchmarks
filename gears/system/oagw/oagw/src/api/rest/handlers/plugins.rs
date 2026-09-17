//! Custom plugin management handlers.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::Uri;
use axum::response::IntoResponse;
use serde_json::Value;
use toolkit::api::response::{created_json, no_content, ok_json};
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{CreatePluginRequest, PluginDto, PluginSourceDto};
use crate::api::rest::error::OagwError;
use crate::api::rest::handlers::{list_page, parse_body};
use crate::domain::model::Plugin;
use crate::domain::services::OagwService;
use crate::gts_helpers::{OagwResourceKind, gts_to_resource_id, resource_id_to_gts};

/// Creates a custom plugin.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn create_plugin(
    Extension(service): Extension<Arc<OagwService>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, OagwError> {
    let request: CreatePluginRequest = parse_body(body)?;
    let plugin = service
        .control_plane()
        .create_plugin(ctx.subject_tenant_id(), request.spec)?;
    let gts_id = resource_id_to_gts(OagwResourceKind::Plugin, plugin.id);
    Ok(created_json(PluginDto::from(&plugin), &uri, &gts_id))
}

/// Lists the custom plugins of the calling tenant.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn list_plugins(
    Extension(service): Extension<Arc<OagwService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, OagwError> {
    let rows = service
        .control_plane()
        .list_plugins(ctx.subject_tenant_id())?
        .iter()
        .map(PluginDto::from)
        .collect();
    Ok(ok_json(list_page(&query, rows)?))
}

/// Reads a single custom plugin.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn get_plugin(
    Extension(service): Extension<Arc<OagwService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwError> {
    let plugin = plugin_of(&service, &ctx, &id).await?;
    Ok(ok_json(PluginDto::from(&plugin)))
}

/// Reads the Starlark source of a custom plugin.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn get_plugin_source(
    Extension(service): Extension<Arc<OagwService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwError> {
    let plugin = plugin_of(&service, &ctx, &id).await?;
    let spec = &plugin.spec;
    Ok(ok_json(PluginSourceDto {
        id: crate::gts_helpers::resource_id_to_gts(OagwResourceKind::Plugin, plugin.id),
        plugin_type: spec.plugin_type.clone(),
        source_code: spec.source_code.clone().unwrap_or_default(),
    }))
}

/// Deletes a custom plugin, unless a configuration still binds it.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn delete_plugin(
    Extension(service): Extension<Arc<OagwService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwError> {
    let plugin = plugin_of(&service, &ctx, &id).await?;
    service
        .control_plane()
        .delete_plugin(ctx.subject_tenant_id(), plugin.id)?;
    Ok(no_content())
}

async fn plugin_of(
    service: &Arc<OagwService>,
    ctx: &SecurityContext,
    id: &str,
) -> Result<Plugin, OagwError> {
    let id = gts_to_resource_id(OagwResourceKind::Plugin, id)?;
    Ok(service
        .control_plane()
        .get_plugin(ctx.subject_tenant_id(), id)?)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use toolkit_security::SecurityContext;

    use super::*;
    use crate::config::OagwConfig;
    use crate::domain::hierarchy::StaticTenantHierarchy;
    use crate::domain::services::OagwService;

    fn service() -> Arc<OagwService> {
        Arc::new(OagwService::new(
            OagwConfig::default(),
            Arc::new(StaticTenantHierarchy::default()),
        ))
    }

    #[tokio::test]
    async fn an_unknown_plugin_kind_is_a_bad_request() {
        let body = json!({ "name": "p", "plugin_type": "not_a_plugin" });
        let Err(error) = create_plugin(
            Extension(service()),
            Extension(SecurityContext::anonymous()),
            Uri::from_static("/oagw/v1/plugins"),
            Json(body),
        )
        .await
        else {
            panic!("unknown plugin kind");
        };
        assert_eq!(error.0.status(), 400);
    }

    #[tokio::test]
    async fn an_unknown_plugin_id_is_not_found() {
        let Err(error) = get_plugin(
            Extension(service()),
            Extension(SecurityContext::anonymous()),
            Path(uuid::Uuid::new_v4().to_string()),
        )
        .await
        else {
            panic!("unknown plugin");
        };
        assert_eq!(error.0.status(), 404);
    }

    #[tokio::test]
    async fn the_source_endpoint_answers_for_a_plugin_without_source() {
        let service = service();
        let uri = Uri::from_static("/oagw/v1/plugins");
        let body = json!({ "name": "p", "plugin_type": "guard_plugin" });
        let response = create_plugin(
            Extension(Arc::clone(&service)),
            Extension(SecurityContext::anonymous()),
            uri,
            Json(body),
        )
        .await
        .expect("created")
        .into_response();
        let location = response.headers().get("location").expect("location");
        let id = location
            .to_str()
            .expect("ascii")
            .rsplit('/')
            .next()
            .expect("id");
        let response = get_plugin_source(
            Extension(Arc::clone(&service)),
            Extension(SecurityContext::anonymous()),
            Path(id.to_owned()),
        )
        .await
        .expect("source")
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
