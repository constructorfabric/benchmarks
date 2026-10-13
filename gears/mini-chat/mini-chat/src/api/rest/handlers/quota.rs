//! Quota status handler (DESIGN section 3.2, "Quota Status Endpoint").

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::Json;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::QuotaStatusResponse;
use crate::gear::AppState;

/// `GET {prefix}/v1/quota/status`
///
/// # Errors
/// Canonical 403 (authz), 500 (policy plugin, database), 503 (PDP).
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    Ok(Json(st.quota.status(&ctx).await?.into()))
}
