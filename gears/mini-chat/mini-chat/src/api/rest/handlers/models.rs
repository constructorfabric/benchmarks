//! Models API handlers (DESIGN section 3.3).

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{ModelDto, ModelListDto};
use crate::gear::AppState;

/// `GET {prefix}/v1/models`
///
/// # Errors
/// Canonical 403 (authz), 500 (policy plugin).
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
) -> ApiResult<Json<ModelListDto>> {
    let items = st.models.list(&ctx).await?;
    Ok(Json(ModelListDto {
        items: items.iter().map(ModelDto::from).collect(),
    }))
}

/// `GET {prefix}/v1/models/{id}`
///
/// # Errors
/// Canonical 403 (authz), 404 (disabled or unknown model), 500 (policy plugin).
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<ModelDto>> {
    let model = st.models.get(&ctx, &id).await?;
    Ok(Json(ModelDto::from(&model)))
}
