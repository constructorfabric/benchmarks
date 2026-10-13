//! Models API handlers (OWNER: REST CRUD work package).

use std::sync::Arc;

use axum::Extension;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;

use crate::domain::service::Services;

/// `GET /v1/models`.
pub async fn list_models(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
) -> ApiResult<Response> {
    let list = svc.models.list(&ctx).await?;
    Ok(axum::Json(list).into_response())
}

/// `GET /v1/models/{id}` (the id is an opaque string).
pub async fn get_model(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let model = svc.models.get(&ctx, &id).await?;
    Ok(axum::Json(model).into_response())
}
