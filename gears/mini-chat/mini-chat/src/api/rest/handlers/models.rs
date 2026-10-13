//! Models API handlers (DESIGN section 3.3 "Models API").

use axum::extract::Extension;
use toolkit::api::canonical_prelude::{ApiResult, Json};
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;

use crate::api::rest::RestState;
use crate::api::rest::dto::{ModelDto, ModelListDto};

/// `GET {prefix}/v1/models` — enabled catalog models.
///
/// # Errors
///
/// 403 (`AUTHZ_DENIED`), 500 (plugin failure), 503 (PDP unavailable).
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
) -> ApiResult<Json<ModelListDto>> {
    let models = state.models.list_visible(&ctx).await?;
    Ok(Json(models.iter().collect()))
}

/// `GET {prefix}/v1/models/{id}` — one enabled catalog model.
///
/// # Errors
///
/// 403 (`AUTHZ_DENIED`), 404 (unknown or disabled model), 500 (plugin
/// failure), 503 (PDP unavailable).
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ModelDto>> {
    let model = state.models.get_visible(&ctx, &id).await?;
    Ok(Json(ModelDto::from(&model)))
}
