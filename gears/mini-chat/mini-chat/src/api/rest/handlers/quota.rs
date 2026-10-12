//! Quota status handler (DESIGN section 3.2 "Quota Status Endpoint (P1)").

use axum::extract::Extension;
use toolkit::api::canonical_prelude::{ApiResult, JsonBody};
use toolkit_security::SecurityContext;

use crate::api::rest::RestState;
use crate::api::rest::dto::QuotaStatusResponse;

/// `GET {prefix}/v1/quota/status` — the caller's remaining quota per tier
/// and period with the warning flags.
///
/// # Errors
///
/// 403, 500 (plugin or storage failure), 503.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
) -> ApiResult<JsonBody<QuotaStatusResponse>> {
    let status = state.quota.status(&ctx).await?;
    Ok(JsonBody::from(QuotaStatusResponse::from(status)))
}
