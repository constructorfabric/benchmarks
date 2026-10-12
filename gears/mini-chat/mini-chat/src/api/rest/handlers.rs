//! Handlers of the Models API. The caller comes from the `SecurityContext`
//! the api-gateway auth layer injects; without it no handler runs.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path};
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use super::dto::{ModelDto, ModelListDto};
use crate::domain::models::ModelsService;

/// `GET {prefix}/v1/models`
///
/// # Errors
/// Canonical `Problem`: 403 `AUTHZ_DENIED`, 503 when the PDP is unreachable,
/// 500 when the policy plugin fails.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ModelsService>>,
) -> ApiResult<Json<ModelListDto>> {
    let items = service
        .list_models(&ctx)
        .await?
        .into_iter()
        .map(ModelDto::from)
        .collect();
    Ok(Json(ModelListDto { items }))
}

/// `GET {prefix}/v1/models/{id}`
///
/// # Errors
/// Canonical `Problem`: 404 for an unknown or disabled model, otherwise as
/// [`list_models`].
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ModelsService>>,
    Path(id): Path<String>,
) -> ApiResult<Json<ModelDto>> {
    let model = service.get_model(&ctx, &id).await?;
    Ok(Json(ModelDto::from(model)))
}
