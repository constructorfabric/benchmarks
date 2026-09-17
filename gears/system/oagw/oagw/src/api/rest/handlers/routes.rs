//! Route management handlers.

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

use crate::api::rest::dto::{CreateRouteRequest, ReplaceRouteRequest, RouteDto};
use crate::api::rest::error::OagwError;
use crate::api::rest::handlers::{list_page, parse_body};
use crate::domain::services::OagwService;
use crate::gts_helpers::{OagwResourceKind, gts_to_resource_id, resource_id_to_gts};

type Routes = Extension<Arc<OagwService>>;

/// Creates a route attached to an upstream of the calling tenant.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn create_route(
    Extension(service): Routes,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, OagwError> {
    let request: CreateRouteRequest = parse_body(body)?;
    let upstream_id = gts_to_resource_id(OagwResourceKind::Upstream, &request.upstream_id)?;
    let route =
        service
            .control_plane()
            .create_route(ctx.subject_tenant_id(), upstream_id, request.spec)?;
    let gts_id = resource_id_to_gts(OagwResourceKind::Route, route.id);
    Ok(created_json(RouteDto::from(&route), &uri, &gts_id))
}

/// Lists the routes of the calling tenant, optionally by upstream.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn list_routes(
    Extension(service): Routes,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, OagwError> {
    let rows = service
        .control_plane()
        .list_routes(ctx.subject_tenant_id(), None)?
        .iter()
        .map(RouteDto::from)
        .collect();
    Ok(ok_json(list_page(&query, rows)?))
}

/// Reads a single route.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn get_route(
    Extension(service): Routes,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwError> {
    let id = gts_to_resource_id(OagwResourceKind::Route, &id)?;
    let route = service
        .control_plane()
        .get_route(ctx.subject_tenant_id(), id)?;
    Ok(ok_json(RouteDto::from(&route)))
}

/// Replaces a route; its upstream reference stays immutable.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn replace_route(
    Extension(service): Routes,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, OagwError> {
    let id = gts_to_resource_id(OagwResourceKind::Route, &id)?;
    let request: ReplaceRouteRequest = parse_body(body)?;
    let route = service
        .control_plane()
        .replace_route(ctx.subject_tenant_id(), id, request.spec)?;
    Ok(ok_json(RouteDto::from(&route)))
}

/// Deletes a route.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn delete_route(
    Extension(service): Routes,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwError> {
    let id = gts_to_resource_id(OagwResourceKind::Route, &id)?;
    service
        .control_plane()
        .delete_route(ctx.subject_tenant_id(), id)?;
    Ok(no_content())
}

#[cfg(test)]
mod tests {
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
    async fn a_route_for_an_unknown_upstream_is_not_found() {
        let body = json!({
            "upstream_id": format!("gts.cf.core.oagw.upstream.v1~{}", uuid::Uuid::new_v4()),
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        });
        let Err(error) = create_route(
            Extension(service()),
            Extension(SecurityContext::anonymous()),
            Uri::from_static("/oagw/v1/routes"),
            Json(body),
        )
        .await
        else {
            panic!("unknown upstream");
        };
        assert_eq!(error.0.status(), 404);
    }

    #[tokio::test]
    async fn an_unknown_route_id_is_not_found() {
        let Err(error) = get_route(
            Extension(service()),
            Extension(SecurityContext::anonymous()),
            Path(uuid::Uuid::new_v4().to_string()),
        )
        .await
        else {
            panic!("unknown route");
        };
        assert_eq!(error.0.status(), 404);
    }
}
