//! Quota status handler.

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::QuotaStatusResponse;
use crate::domain::services::Services;

/// `GET /v1/quota/status`.
///
/// # Errors
/// Canonical problem for authorization, policy and storage failures.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let status = svc.quota_status.status(&ctx).await?;
    Ok(Json(status.into()))
}
