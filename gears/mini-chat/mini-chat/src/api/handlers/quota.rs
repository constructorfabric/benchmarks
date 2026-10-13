//! `GET {prefix}/v1/quota/status`.

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use crate::api::dto::quota::QuotaStatusResponse;
use crate::api::state::AppServices;

/// `mini_chat.get_quota_status`: the caller's credit usage per tier and period.
///
/// # Errors
/// 403 / 503 from the PDP, 500 when the policy plugin or the database fails.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let status = svc.quota.status(&ctx).await?;
    Ok(Json(status.into()))
}
