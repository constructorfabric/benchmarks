//! Handler of `GET /v1/quota/status`. The caller comes from the
//! `SecurityContext` the api-gateway auth layer injects.

use std::sync::Arc;

use axum::Json;
use axum::extract::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use super::quota_dto::QuotaStatusResponse;
use crate::domain::quota_status::QuotaStatusService;

/// `GET {prefix}/v1/quota/status`
///
/// # Errors
/// Canonical `Problem`: 403 `AUTHZ_DENIED`, 503 when the PDP is unreachable,
/// 500 when the policy plugin or the database fails.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<QuotaStatusService>>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    Ok(Json(service.status(&ctx).await?.into()))
}
