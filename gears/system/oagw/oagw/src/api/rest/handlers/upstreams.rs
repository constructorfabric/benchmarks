//! Upstream management handlers.

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

use crate::api::rest::dto::{CreateUpstreamRequest, ReplaceUpstreamRequest, UpstreamDto};
use crate::api::rest::error::OagwError;
use crate::api::rest::handlers::{list_page, parse_body};
use crate::domain::services::OagwService;
use crate::gts_helpers::{OagwResourceKind, gts_to_resource_id};

type Upstreams = Extension<Arc<OagwService>>;

/// Creates an upstream; the alias is derived when omitted.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn create_upstream(
    Extension(service): Upstreams,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, OagwError> {
    let request: CreateUpstreamRequest = parse_body(body)?;
    let upstream = service
        .control_plane()
        .create_upstream(&ctx, ctx.subject_tenant_id(), request.spec)
        .await?;
    Ok(created_json(
        UpstreamDto::from(&upstream),
        &uri,
        &upstream.gts_id(),
    ))
}

/// Lists the upstreams of the calling tenant.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn list_upstreams(
    Extension(service): Upstreams,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, OagwError> {
    let rows = service
        .control_plane()
        .list_upstreams(ctx.subject_tenant_id())?
        .iter()
        .map(UpstreamDto::from)
        .collect();
    Ok(ok_json(list_page(&query, rows)?))
}

/// Reads a single upstream.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn get_upstream(
    Extension(service): Upstreams,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwError> {
    let id = gts_to_resource_id(OagwResourceKind::Upstream, &id)?;
    let upstream = service
        .control_plane()
        .get_upstream(ctx.subject_tenant_id(), id)?;
    Ok(ok_json(UpstreamDto::from(&upstream)))
}

/// Replaces an upstream in place.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn replace_upstream(
    Extension(service): Upstreams,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, OagwError> {
    let id = gts_to_resource_id(OagwResourceKind::Upstream, &id)?;
    let request: ReplaceUpstreamRequest = parse_body(body)?;
    let upstream = service
        .control_plane()
        .replace_upstream(&ctx, ctx.subject_tenant_id(), id, request.spec)
        .await?;
    Ok(ok_json(UpstreamDto::from(&upstream)))
}

/// Deletes an upstream and its routes.
///
/// # Errors
/// Renders [`crate::domain::error::DomainError`] as the gear's problem body.
pub async fn delete_upstream(
    Extension(service): Upstreams,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwError> {
    let id = gts_to_resource_id(OagwResourceKind::Upstream, &id)?;
    service
        .control_plane()
        .delete_upstream(ctx.subject_tenant_id(), id)?;
    Ok(no_content())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

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
    async fn creates_an_upstream_with_a_derived_alias() {
        let body = json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] }
        });
        let response = create_upstream(
            Extension(service()),
            Extension(SecurityContext::anonymous()),
            Uri::from_static("/oagw/v1/upstreams"),
            Json(body),
        )
        .await
        .expect("created");
        let response = response.into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(response.headers().get("location").is_some());
    }

    #[tokio::test]
    async fn an_ip_endpoint_without_an_alias_is_a_bad_request() {
        let body = json!({
            "server": { "endpoints": [{ "scheme": "http", "host": "10.0.0.1", "port": 8080 }] }
        });
        let Err(error) = create_upstream(
            Extension(service()),
            Extension(SecurityContext::anonymous()),
            Uri::from_static("/oagw/v1/upstreams"),
            Json(body),
        )
        .await
        else {
            panic!("missing alias");
        };
        assert_eq!(error.0.status(), 400);
    }

    #[tokio::test]
    async fn an_unknown_id_is_not_found() {
        let Err(error) = get_upstream(
            Extension(service()),
            Extension(SecurityContext::anonymous()),
            Path(Uuid::new_v4().to_string()),
        )
        .await
        else {
            panic!("unknown upstream");
        };
        assert_eq!(error.0.status(), 404);
    }
}
