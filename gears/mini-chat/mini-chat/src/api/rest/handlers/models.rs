//! Models and quota status handlers.

use super::prelude::*;

/// `GET /v1/models` — models available to the caller.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn list_models(Extension(ctx): Extension<SecurityContext>, Extension(svc): Svc) -> ApiResult<Json<ModelListDto>> {
    let items = svc.list_models(&ctx).await.map_err(err)?;
    Ok(Json(ModelListDto { items: items.into_iter().map(ModelDto::from).collect() }))
}

/// `GET /v1/models/{id}` — one model.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    Ok(Json(svc.get_model(&ctx, &id).await.map_err(err)?.into()))
}

/// `GET /v1/quota/status` — the caller's quota status.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn get_quota_status(Extension(ctx): Extension<SecurityContext>, Extension(svc): Svc) -> ApiResult<Json<QuotaStatusResponse>> {
    Ok(Json(svc.quota_status(&ctx).await.map_err(err)?.into()))
}
