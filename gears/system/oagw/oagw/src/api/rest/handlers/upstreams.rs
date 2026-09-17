//! Management handlers for `/oagw/v1/upstreams`.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{
    EndpointDto, ServerDto, UpsertUpstreamDto, UpstreamDto, upstream_from_dto,
};
use crate::domain::model::Endpoint;
use crate::domain::repo::ListFilter;
use crate::domain::service::ControlPlaneService;

/// Extracts and validates the endpoint pool from the wire body.
///
/// # Errors
///
/// Returns 400 when any endpoint carries an unknown scheme.
pub fn endpoints_from_dto(
    server: &ServerDto,
) -> Result<Vec<Endpoint>, crate::domain::error::DomainError> {
    server
        .endpoints
        .clone()
        .into_iter()
        .map(EndpointDto::into_endpoint)
        .collect()
}

/// `POST /oagw/v1/upstreams`
///
/// # Errors
///
/// Returns 400 on validation failure and 409 on alias collision.
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Json(body): Json<UpsertUpstreamDto>,
) -> Result<(StatusCode, Json<UpstreamDto>), crate::domain::error::DomainError> {
    let endpoints = endpoints_from_dto(&body.server)?;
    let created = svc
        .create_upstream(
            ctx.subject_tenant_id(),
            endpoints,
            body.alias.clone(),
            body.tags.clone(),
            body.protocol,
            upstream_from_dto(&body),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(UpstreamDto::from(created))))
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
///
/// Propagates store failures.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Query(filter): Query<ListQuery>,
) -> Result<Json<Vec<UpstreamDto>>, crate::domain::error::DomainError> {
    let rows = svc
        .list_upstreams(ctx.subject_tenant_id(), &filter.into_filter())
        .await?;
    Ok(Json(rows.into_iter().map(UpstreamDto::from).collect()))
}

/// Query parameters shared by the list endpoints.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ListQuery {
    /// Exact-match alias filter (upstreams).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Restrict to an owning upstream (routes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<uuid::Uuid>,
    /// Restrict to a plugin kind (plugins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Maximum number of rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<usize>,
    /// Number of rows to skip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<usize>,
}

impl ListQuery {
    /// Converts to the repository filter.
    #[must_use]
    pub fn into_filter(self) -> ListFilter {
        ListFilter {
            alias: self.alias,
            upstream_id: self.upstream_id,
            plugin_type: self.plugin_type,
            top: self.top,
            skip: self.skip,
        }
    }
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns 404 when the upstream is absent or owned by another tenant.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<UpstreamDto>, crate::domain::error::DomainError> {
    Ok(Json(UpstreamDto::from(
        svc.get_upstream(ctx.subject_tenant_id(), id).await?,
    )))
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns 404 when absent, 400 on an illegal alias transition and 409 on a
/// sibling alias collision.
pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<UpsertUpstreamDto>,
) -> Result<Json<UpstreamDto>, crate::domain::error::DomainError> {
    let endpoints = endpoints_from_dto(&body.server)?;
    let updated = svc
        .replace_upstream(
            ctx.subject_tenant_id(),
            id,
            endpoints,
            body.alias.clone(),
            body.tags.clone(),
            upstream_from_dto(&body),
        )
        .await?;
    Ok(Json(UpstreamDto::from(updated)))
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns 404 when absent and 409 while routes still reference it.
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
) -> Result<impl IntoResponse, crate::domain::error::DomainError> {
    svc.delete_upstream(ctx.subject_tenant_id(), id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_pool_conversion_collects_all_errors() {
        let server = ServerDto {
            endpoints: vec![
                EndpointDto {
                    scheme: "https".to_owned(),
                    host: "api.example".to_owned(),
                    port: None,
                },
                EndpointDto {
                    scheme: "gopher".to_owned(),
                    host: "api2.example".to_owned(),
                    port: Some(443),
                },
            ],
        };
        let err = endpoints_from_dto(&server).expect_err("unknown scheme");
        assert!(format!("{err}").contains("unknown endpoint scheme 'gopher'"));
    }

    #[test]
    fn list_query_maps_into_filter() {
        let query = ListQuery {
            alias: Some("api.example".to_owned()),
            upstream_id: Some(uuid::Uuid::nil()),
            plugin_type: Some("guard".to_owned()),
            top: Some(10),
            skip: Some(5),
        };
        let filter = query.into_filter();
        assert_eq!(filter.alias.as_deref(), Some("api.example"));
        assert_eq!(filter.top, Some(10));
        assert_eq!(filter.skip, Some(5));
    }
}
