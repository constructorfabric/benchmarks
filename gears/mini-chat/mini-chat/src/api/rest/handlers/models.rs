//! Models API handlers.

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{ModelDto, ModelListDto};
use crate::domain::services::Services;

/// `GET /v1/models`.
///
/// # Errors
/// Canonical problem for authorization and policy failures.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
) -> ApiResult<Json<ModelListDto>> {
    let items = svc.models.list(&ctx).await?;
    Ok(Json(ModelListDto {
        items: items.into_iter().map(ModelDto::from).collect(),
    }))
}

/// `GET /v1/models/{id}`.
///
/// # Errors
/// Canonical problem for authorization failures and unknown or disabled models.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    let model = svc.models.get(&ctx, &id).await?;
    Ok(Json(model.into()))
}
