//! Upstream management handlers (`/api/oagw/v1/upstreams`).

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, RawQuery};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::{Json, created_json, no_content};
use toolkit_security::context::SecurityContext;
use super::common;
use crate::api::rest::dto::{
    CreateUpstreamRequest, PluginsConfigDto, ReplaceUpstreamRequest, UpstreamDto,
};
use crate::api::rest::query::ListQuery;
use crate::domain::error::DomainError;
use crate::domain::models::{Upstream, UPSTREAM_TYPE};
use crate::domain::service::{ControlPlaneService, UpstreamDraft};

/// Service extension type shared by every handler.
pub type Service = Arc<ControlPlaneService>;

/// Converts a request payload into a domain draft.
#[must_use]
pub fn upstream_draft(request: &CreateUpstreamRequest) -> UpstreamDraft {
    UpstreamDraft {
        alias: request.alias.clone(),
        enabled: request.enabled,
        protocol: request.protocol,
        server: request.server.clone(),
        auth: request.auth.clone(),
        headers: request.headers.clone(),
        plugins: request.plugins.clone().map(PluginsConfigDto::into_domain),
        rate_limit: request.rate_limit.clone(),
        cors: request.cors.clone(),
        tags: request.tags.clone(),
    }
}

/// Converts a replacement payload into a domain draft.
#[must_use]
pub fn replace_draft(request: &ReplaceUpstreamRequest) -> UpstreamDraft {
    UpstreamDraft {
        alias: request.alias.clone(),
        enabled: request.enabled,
        protocol: request.protocol,
        server: request.server.clone(),
        auth: request.auth.clone(),
        headers: request.headers.clone(),
        plugins: request.plugins.clone().map(PluginsConfigDto::into_domain),
        rate_limit: request.rate_limit.clone(),
        cors: request.cors.clone(),
        tags: request.tags.clone(),
    }
}

/// `POST /upstreams` — 201 with the created upstream.
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
    axum::Json(request): axum::Json<CreateUpstreamRequest>,
) -> Result<Response, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let upstream = service
        .create_upstream(tenant_id, upstream_draft(&request))
        .await?;
    let id = common::resource_id(UPSTREAM_TYPE, upstream.id);
    Ok(created_json(UpstreamDto::from(upstream), &uri, &id).into_response())
}

/// `GET /upstreams`
pub async fn list_upstreams(
    RawQuery(query): RawQuery,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<Json<toolkit_odata::Page<serde_json::Value>>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let query = ListQuery::parse(query.as_deref())?;
    let items = service.list_upstreams(tenant_id).await?;
    common::paged(&items, &query, |upstream| {
        serde_json::to_value(UpstreamDto::from(upstream.clone())).unwrap_or(serde_json::Value::Null)
    })
}

/// `GET /upstreams/{id}`
pub async fn get_upstream(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<Json<UpstreamDto>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = common::parse_resource_id(UPSTREAM_TYPE, &raw_id)?;
    let upstream = service.get_upstream(tenant_id, id).await?;
    Ok(Json(UpstreamDto::from(upstream)))
}

/// `PUT /upstreams/{id}`
pub async fn replace_upstream(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
    axum::Json(request): axum::Json<ReplaceUpstreamRequest>,
) -> Result<Json<UpstreamDto>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = common::parse_resource_id(UPSTREAM_TYPE, &raw_id)?;
    let upstream = service
        .replace_upstream(tenant_id, id, replace_draft(&request))
        .await?;
    Ok(Json(UpstreamDto::from(upstream)))
}

/// `DELETE /upstreams/{id}` — cascades to the upstream's routes.
pub async fn delete_upstream(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<impl IntoResponse, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = common::parse_resource_id(UPSTREAM_TYPE, &raw_id)?;
    service.delete_upstream(tenant_id, id).await?;
    Ok(no_content())
}

/// Serializes an upstream DTO (kept for tests and the list projection).
#[must_use]
pub fn upstream_json(upstream: &Upstream) -> serde_json::Value {
    serde_json::to_value(UpstreamDto::from(upstream.clone())).unwrap_or(serde_json::Value::Null)
}

