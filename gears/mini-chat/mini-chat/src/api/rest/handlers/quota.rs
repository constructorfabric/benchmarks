//! Quota status handler (`GET /v1/quota/status`).

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::{ApiResult, Json};
use toolkit_security::SecurityContext;

use crate::api::rest::dto::QuotaStatusResponse;
use crate::domain::services::AppServices;

/// `GET /v1/quota/status` — per-tier, per-period quota of the caller.
///
/// # Errors
/// 403 / 503 from authorization; 500 on policy plugin or database failure.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let status = svc.quota.status(&ctx).await?;
    Ok(Json(status.into()))
}
