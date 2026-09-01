//! REST handlers for tenant metadata. PEP gate lives on
//! [`MetadataService`]; handlers extract [`SecurityContext`] + service
//! handle and forward. `DomainError` → `CanonicalError` via the
//! `From` impl in [`crate::infra::sdk_error_mapping`].

use std::sync::Arc;

use axum::Extension;
use axum::extract::Path;
use axum::response::IntoResponse;
use gts::GtsTypeId;
use tracing::field::Empty;
use uuid::Uuid;

use account_management_sdk::UpsertMetadataRequest;
use toolkit::api::canonical_prelude::*;
use toolkit::api::odata::OData;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{
    PutTenantMetadataDto, ResolvedTenantMetadataDto, TenantMetadataEntryDto,
};
use crate::api::rest::handlers::common::clamp_listing_top;
use crate::domain::metadata::service::MetadataService;

/// `GET /account-management/v1/tenants/{tenant_id}/metadata`
///
/// Direct-on-tenant listing only. Inherited values are NOT included —
/// use [`resolve_metadata`] for effective values.
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `cross_tenant_denied` (403), tenant `not_found` (404), and
/// `service_unavailable` (503) on PDP / DB transport failure.
#[tracing::instrument(
    skip(svc, ctx, query),
    fields(tenant_id = %tenant_id, request_id = Empty)
)]
pub async fn list_metadata(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MetadataService>>,
    Path(tenant_id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<toolkit_odata::Page<TenantMetadataEntryDto>>> {
    let query = clamp_listing_top(query, svc.max_listing_top());
    let page = svc.list_metadata(&ctx, tenant_id, &query).await?;
    Ok(Json(page.map_items(|entry| {
        TenantMetadataEntryDto::from_entry(tenant_id, entry)
    })))
}

/// `GET /account-management/v1/tenants/{tenant_id}/metadata/{type_id}`
///
/// Unified 404: both "schema unknown to types-registry" and
/// "schema registered but no entry for this tenant" surface as
/// `code=not_found` with `resource_type =
/// gts.cf.core.am.tenant_metadata.v1~`. The envelope's
/// `resource_name` carries the chained `type_id` from the URL.
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `metadata_validation` (400 -- malformed chained `type_id`),
/// `cross_tenant_denied` (403), `not_found` (404 -- unified
/// metadata 404), `service_unavailable` (503).
#[tracing::instrument(
    skip(svc, ctx),
    fields(tenant_id = %tenant_id, type_id = %type_id_raw, request_id = Empty)
)]
pub async fn get_metadata(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MetadataService>>,
    Path((tenant_id, type_id_raw)): Path<(Uuid, String)>,
) -> ApiResult<Json<TenantMetadataEntryDto>> {
    let type_id = GtsTypeId::new(&type_id_raw);
    let entry = svc.get_metadata(&ctx, tenant_id, type_id).await?;
    Ok(Json(TenantMetadataEntryDto::from_entry(tenant_id, entry)))
}

/// `PUT /account-management/v1/tenants/{tenant_id}/metadata/{type_id}`
///
/// Always returns HTTP 200 (no 201 on create) per RFC 7231 PUT
/// semantics; insert-vs-update is preserved on the
/// `am.events:metadata_upserted` audit line (`outcome=created`/`updated`).
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `metadata_validation` (400 -- malformed `type_id`, null `value`,
/// or body fails the registered JSON Schema), `cross_tenant_denied`
/// (403), `not_found` (404 -- schema not in registry; unified
/// metadata 404), `service_unavailable` (503 -- types-registry or
/// authz-resolver unreachable), `internal` (500 -- registered schema
/// is not a valid JSON Schema).
#[tracing::instrument(
    skip(svc, ctx, req_body),
    fields(tenant_id = %tenant_id, type_id = %type_id_raw, request_id = Empty)
)]
pub async fn upsert_metadata(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MetadataService>>,
    Path((tenant_id, type_id_raw)): Path<(Uuid, String)>,
    Json(req_body): Json<PutTenantMetadataDto>,
) -> ApiResult<Json<TenantMetadataEntryDto>> {
    let type_id = GtsTypeId::new(&type_id_raw);
    let request = UpsertMetadataRequest::new(type_id, req_body.value);
    let entry = svc.upsert_metadata(&ctx, tenant_id, request).await?;
    Ok(Json(TenantMetadataEntryDto::from_entry(tenant_id, entry)))
}

/// `DELETE /account-management/v1/tenants/{tenant_id}/metadata/{type_id}`
///
/// Idempotent on missing rows: returns 204 whether the direct entry
/// existed and was removed or was already absent (mirrors `delete_user`
/// deprovision idempotency). Tenant-existence and schema-registration
/// gates still surface their own 404 codes.
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `metadata_validation` (400 -- malformed `type_id`),
/// `cross_tenant_denied` (403), `not_found` (404 -- schema not in
/// registry; unified metadata 404), `service_unavailable` (503).
#[tracing::instrument(
    skip(svc, ctx),
    fields(tenant_id = %tenant_id, type_id = %type_id_raw, request_id = Empty)
)]
pub async fn delete_metadata(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MetadataService>>,
    Path((tenant_id, type_id_raw)): Path<(Uuid, String)>,
) -> ApiResult<impl IntoResponse> {
    let type_id = GtsTypeId::new(&type_id_raw);
    svc.delete_metadata(&ctx, tenant_id, type_id).await?;
    Ok(no_content().into_response())
}

/// `GET /account-management/v1/tenants/{tenant_id}/metadata/{type_id}/resolved`
///
/// An empty walk maps to `resolved=false` (HTTP 200), NOT a 404, per
/// FEATURE §3.
///
/// # Errors
///
/// Surfaces a canonical `Problem` envelope. Notable codes:
/// `metadata_validation` (400 -- malformed `type_id`),
/// `cross_tenant_denied` (403), `not_found` (404 -- tenant missing
/// OR schema not in registry; unified metadata 404),
/// `service_unavailable` (503), `internal` (500 -- ancestor walk hit
/// a `parent_id` cycle or dangling-parent reference).
#[tracing::instrument(
    skip(svc, ctx),
    fields(tenant_id = %tenant_id, type_id = %type_id_raw, request_id = Empty)
)]
pub async fn resolve_metadata(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MetadataService>>,
    Path((tenant_id, type_id_raw)): Path<(Uuid, String)>,
) -> ApiResult<Json<ResolvedTenantMetadataDto>> {
    let type_id = GtsTypeId::new(&type_id_raw);
    let echoed = type_id.as_ref().to_owned();
    let resolution = svc.resolve_metadata(&ctx, tenant_id, type_id).await?;
    Ok(Json(ResolvedTenantMetadataDto::from_resolution(
        tenant_id, echoed, resolution,
    )))
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
