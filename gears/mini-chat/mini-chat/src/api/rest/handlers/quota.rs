//! Quota status handler (OWNER: quota & billing work package).

use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit_security::SecurityContext;

use crate::domain::service::Services;

/// `GET /v1/quota/status`.
pub async fn get_quota_status(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
) -> ApiResult<Response> {
    let status = svc.quota.quota_status(&ctx).await?;
    Ok(Json(status).into_response())
}
