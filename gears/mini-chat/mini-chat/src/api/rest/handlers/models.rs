//! Models API handlers (`GET /v1/models`, `GET /v1/models/{id}`).

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::{ApiResult, Json};
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{ModelDto, ModelListDto};
use crate::domain::authz;
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;

/// `GET /v1/models` — globally enabled models, catalog order.
///
/// # Errors
/// 403 / 503 from authorization; 500 on policy plugin failure.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
) -> ApiResult<Json<ModelListDto>> {
    svc.pep.model_access(&ctx, authz::LIST).await?;
    let models = svc.models.visible_models(ctx.subject_id()).await?;
    Ok(Json(ModelListDto {
        items: models.iter().map(ModelDto::from).collect(),
    }))
}

/// `GET /v1/models/{id}` — one enabled model.
///
/// # Errors
/// 404 (`model`) when disabled or unknown; 403 / 503 from authorization;
/// 500 on policy plugin failure.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<String>,
) -> ApiResult<Json<ModelDto>> {
    svc.pep.model_access(&ctx, authz::READ).await?;
    let models = svc.models.visible_models(ctx.subject_id()).await?;
    let model = models
        .iter()
        .find(|m| m.id == id)
        .ok_or(DomainError::ModelNotFound)?;
    Ok(Json(ModelDto::from(model)))
}
