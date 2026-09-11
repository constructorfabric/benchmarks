//! Plugin management handlers.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, RawQuery};
use axum::http::StatusCode;
use axum::response::Response;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{ListParams, ListResponse, PluginDto, PluginSourceDto};
use crate::api::rest::handlers::upstreams::{instance, into_json, no_content, problem};
use crate::api::rest::state::OagwState;
use crate::domain::error::DomainError;
use crate::infra::authz::TenantChain;

/// A create-plugin request body.
#[derive(Debug, serde::Deserialize)]
pub struct CreatePluginRequest {
    /// Plugin kind (`auth` | `guard` | `transform`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Plugin GTS type, e.g. `gts.cf.core.oagw.guard_plugin.v1~{uuid}`.
    #[serde(default)]
    pub plugin_type: Option<String>,
    /// Human-readable name, unique per tenant.
    pub name: String,
    /// Declarative configuration.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// The plugin source (Starlark).
    #[serde(default)]
    pub source: Option<String>,
}

/// Maps a plugin kind from a request body.
fn kind_of(kind: Option<&str>, plugin_type: Option<&str>) -> Result<crate::domain::repo::PluginKind, DomainError> {
    use crate::domain::repo::PluginKind;
    if let Some(plugin_type) = plugin_type {
        return PluginKind::from_gts_type(plugin_type).ok_or_else(|| DomainError::ValidationError {
            detail: format!("`{plugin_type}` is not an oagw plugin type"),
        });
    }
    match kind.as_deref().map(str::trim) {
        Some("auth") => Ok(PluginKind::Auth),
        Some("guard") => Ok(PluginKind::Guard),
        Some("transform") => Ok(PluginKind::Transform),
        _ => Err(DomainError::ValidationError {
            detail: "`kind` must be one of auth, guard, transform".to_string(),
        }),
    }
}

/// Renders a plugin record.
fn plugin_dto(record: &crate::domain::repo::PluginRecord) -> PluginDto {
    PluginDto {
        id: record.id.to_string(),
        gts_id: format!("{}{}", record.kind.gts_type(), record.id),
        kind: match record.kind {
            crate::domain::repo::PluginKind::Auth => "auth".to_string(),
            crate::domain::repo::PluginKind::Guard => "guard".to_string(),
            crate::domain::repo::PluginKind::Transform => "transform".to_string(),
        },
        name: record.name.clone(),
        config: Some(record.config.clone()),
    }
}

/// Creates a plugin.
pub async fn create_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(request): Json<CreatePluginRequest>,
) -> Response {
    let instance_path = instance("/plugins");
    let kind = match kind_of(request.kind.as_deref(), request.plugin_type.as_deref()) {
        Ok(k) => k,
        Err(err) => return problem(err, &instance_path),
    };
    let config = request.config.unwrap_or(serde_json::Value::Null);
    match state
        .control_plane
        .create_plugin(
            ctx.subject_tenant_id(),
            kind,
            &request.name,
            config,
            request.source.as_deref().unwrap_or_default(),
        )
        .await
    {
        Ok(record) => {
            let body = plugin_dto(&record);
            axum::response::Response::builder()
                .status(StatusCode::CREATED)
                .header(
                    axum::http::header::LOCATION,
                    format!("/oagw/v1/plugins/{}", record.id),
                )
                .header("X-OAGW-Error-Source", "gateway")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap_or_default()))
                .expect("static response")
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Lists plugins.
pub async fn list_plugins(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    RawQuery(raw): RawQuery,
) -> Response {
    let instance_path = instance("/plugins");
    let params = ListParams::from_query(&crate::api::rest::extractors::parse_query(
        raw.as_deref().unwrap_or_default(),
    ));
    match state.control_plane.list_plugins(ctx.subject_tenant_id()).await {
        Ok(all) => {
            let all = crate::api::rest::dto::apply_filter(all, params.filter.as_deref(), |p| {
                p.name.clone()
            });
            let (page, total) = params.paginate(all);
            let page: Vec<PluginDto> = page.iter().map(plugin_dto).collect();
            into_json(
                serde_json::to_value(ListResponse::new(page, Some(total), None)).unwrap_or_default(),
            )
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Reads a plugin by id.
pub async fn get_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let instance_path = instance(&format!("/plugins/{id}"));
    match state.control_plane.get_plugin(ctx.subject_tenant_id(), &id).await {
        Ok(record) => into_json(serde_json::to_value(plugin_dto(&record)).unwrap_or_default()),
        Err(err) => problem(err, &instance_path),
    }
}

/// Returns the plugin's stored source.
pub async fn get_plugin_source(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let instance_path = instance(&format!("/plugins/{id}/source"));
    match state.control_plane.get_plugin(ctx.subject_tenant_id(), &id).await {
        Ok(record) => into_json(
            serde_json::to_value(PluginSourceDto {
                id: record.id.to_string(),
                source: record.source,
            })
            .unwrap_or_default(),
        ),
        Err(err) => problem(err, &instance_path),
    }
}

/// Deletes a plugin; a delete of a bound plugin reports 409 with its references.
pub async fn delete_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let instance_path = instance(&format!("/plugins/{id}"));
    let chain = TenantChain::from_entries(vec![ctx.subject_tenant_id()]);
    match state.control_plane.delete_plugin(ctx.subject_tenant_id(), &chain, &id).await {
        Ok(()) => no_content(),
        Err(err) => problem(err, &instance_path),
    }
}
