//! Plugin-catalog handlers.
//!
//! The catalog lists what an operator may bind: the plugins this build ships,
//! plus the entries an operator filed themselves. A filed entry is immutable —
//! a change is a new plugin, re-bound by the routes that want it — and it can
//! only be retired once no route is using it.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::api::rest::dto::{PluginDto, PluginListDto};
use crate::api::rest::error::{ApiResult, GatewayProblem};
use crate::api::rest::extractors::{Action, AuthenticatedSubject, require_permission};
use crate::domain::services::control_plane::ControlPlaneService;
use crate::gts_helpers::TRANSFORM_PLUGIN_GTS_ID;

/// `GET /oagw/v1/plugins` — the plugin catalog.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
pub async fn list_plugins(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
) -> ApiResult<Json<PluginListDto>> {
    require_permission(subject.context(), Action::Read, TRANSFORM_PLUGIN_GTS_ID)?;

    let entries: Vec<PluginDto> = svc
        .plugins(subject.context())
        .await
        .into_iter()
        .map(PluginDto::from)
        .collect();
    let total_count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    Ok(Json(PluginListDto {
        items: entries,
        total_count,
    }))
}

/// `GET /oagw/v1/plugins/{id}` — one catalog entry.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
pub async fn get_plugin(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginDto>> {
    require_permission(subject.context(), Action::Read, TRANSFORM_PLUGIN_GTS_ID)?;

    let descriptor = svc.plugin(subject.context(), &id).await.ok_or_else(|| {
        GatewayProblem::from(crate::domain::error::DomainError::not_found(format!(
            "plugin '{id}' is not in the catalog"
        )))
    })?;
    Ok(Json(PluginDto::from(descriptor)))
}

/// `GET /oagw/v1/plugins/{id}/source` — a custom plugin's source, verbatim.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - `NotFound` when no custom plugin of that identifier is in scope
pub async fn get_plugin_source(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> Result<Response, GatewayProblem> {
    require_permission(subject.context(), Action::Read, TRANSFORM_PLUGIN_GTS_ID)?;

    let plugin = svc.plugin_source(subject.context(), &id).await?;
    // `text/plain` and nothing else: the source is exactly the bytes it was
    // filed with, so an operator can diff it or hand it to their build.
    let body = axum::body::Body::from(plugin.source.into_bytes());
    let content_type = axum::http::HeaderValue::from_static("text/plain; charset=utf-8");
    let response = (http::StatusCode::OK, [(CONTENT_TYPE, content_type)], body).into_response();
    Ok(response)
}

/// What `POST /oagw/v1/plugins` accepts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCreateRequest {
    /// Identifier the operator chose.
    pub id: String,
    /// Class the plugin belongs to.
    #[serde(default)]
    pub plugin_type: crate::domain::plugin::PluginType,
    /// Operator-facing description.
    #[serde(default)]
    pub description: String,
    /// The plugin's source.
    pub source: String,
}

/// `POST /oagw/v1/plugins` — file a tenant-defined plugin in the catalog.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - `Validation` when the identifier or the source is unusable
/// - `AliasConflict` when the identifier is already taken
pub async fn create_plugin(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    request: axum::Json<PluginCreateRequest>,
) -> ApiResult<impl IntoResponse> {
    require_permission(subject.context(), Action::Write, TRANSFORM_PLUGIN_GTS_ID)?;

    let filed = svc
        .create_plugin(
            subject.context(),
            &request.id,
            request.plugin_type,
            request.description.clone(),
            request.source.clone(),
        )
        .await?;
    Ok((
        http::StatusCode::CREATED,
        Json(PluginDto {
            id: filed.id,
            plugin_type: filed.plugin_type,
            version: "1".to_owned(),
            description: filed.description,
            built_in: false,
        }),
    ))
}

/// `PUT /oagw/v1/plugins/{id}` — refused, because a plugin is immutable.
/// # Errors
///
/// Always `Validation`: a change is a new plugin version, re-bound by the
/// routes that want it (PRD 5.3).
pub async fn replace_plugin(Path(id): Path<String>) -> Response {
    let error = crate::domain::error::DomainError::validation(format!(
        "plugin '{id}' is immutable: file a new version and re-bind it"
    ));
    GatewayProblem::from(error).into_response()
}

/// `DELETE /oagw/v1/plugins/{id}` — retire a plugin no route binds.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier is not in the catalog
/// - `PluginInUse` when a route still binds it
pub async fn delete_plugin(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    require_permission(subject.context(), Action::Delete, TRANSFORM_PLUGIN_GTS_ID)?;

    svc.remove_plugin(subject.context(), &id).await?;
    Ok(http::StatusCode::NO_CONTENT)
}
