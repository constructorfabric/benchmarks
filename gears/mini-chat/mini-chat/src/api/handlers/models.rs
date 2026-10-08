//! Models API handlers (DESIGN §3.3 "Models API"): enabled catalog entries only, no internal fields.

use std::sync::Arc;

use axum::Extension;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::{ApiResult, StatusCode};
use toolkit::api::operation_builder::{OperationBuilder, ResponseHeaderSpec, ResponseHeaderType};
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;

use super::{License, V1};
use crate::domain::models;
use crate::domain::services::AppServices;

const TAG: &str = "Mini Chat Models";

/// Model tier.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

/// Response DTO for a single model.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelDto {
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTierDto,
    pub multiplier_display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

impl From<ModelCatalogEntry> for ModelDto {
    fn from(m: ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id,
            display_name: m.display_name,
            tier: match m.tier {
                ModelTier::Premium => ModelTierDto::Premium,
                ModelTier::Standard => ModelTierDto::Standard,
            },
            multiplier_display: m.multiplier_display,
            description: Some(m.description).filter(|d| !d.is_empty()),
            multimodal_capabilities: m.multimodal_capabilities,
            context_window: m.context_window,
        }
    }
}

/// Response DTO for the model list endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

/// `GET /models`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
) -> ApiResult<axum::Json<ModelListDto>> {
    let items = models::list_models(&app, &ctx).await?.into_iter().map(ModelDto::from).collect();
    Ok(axum::Json(ModelListDto { items }))
}

/// `GET /models/{id}`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path(id): Path<String>,
) -> ApiResult<axum::Json<ModelDto>> {
    Ok(axum::Json(models::get_model(&app, &ctx, &id).await?.into()))
}

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new("Retry-After", "Seconds to wait before retrying", ResponseHeaderType::Integer)
}

/// Registers this area's routes.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = OperationBuilder::get(format!("{V1}/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .handler(list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    OperationBuilder::get(format!("{V1}/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Model identifier")
        .handler(get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
