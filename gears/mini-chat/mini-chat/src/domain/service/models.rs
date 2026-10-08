//! Models API (OWNER: REST CRUD work package).
//!
//! Read-only projection of the enabled entries of the policy catalog (DESIGN §3.3 Models API).

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{ModelDto, ModelListDto, ModelTierDto};
use crate::domain::authz;
use crate::domain::error::{DomainError, resource_types};
use crate::domain::service::Deps;

/// Projects a catalog entry onto the public model DTO (no provider / pricing internals).
#[must_use]
pub fn model_to_dto(m: &ModelCatalogEntry) -> ModelDto {
    ModelDto {
        model_id: m.id.clone(),
        display_name: m.display_name.clone(),
        tier: match m.tier {
            ModelTier::Standard => ModelTierDto::Standard,
            ModelTier::Premium => ModelTierDto::Premium,
        },
        multiplier_display: m.multiplier_display.clone(),
        description: (!m.description.is_empty()).then(|| m.description.clone()),
        multimodal_capabilities: m.multimodal_capabilities.clone(),
        context_window: m.context_window,
    }
}

pub struct ModelService {
    deps: Arc<Deps>,
}

impl ModelService {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    /// `GET /v1/models`: enabled catalog entries in catalog order.
    ///
    /// # Errors
    /// 403/503 from the PEP, 500 on plugin failure.
    pub async fn list(&self, ctx: &SecurityContext) -> Result<ModelListDto, DomainError> {
        authz::model_permission(&self.deps.enforcer, ctx, authz::actions::LIST).await?;
        let snapshot = self.deps.policy.current_snapshot(ctx.subject_id()).await?;
        let items = snapshot
            .model_catalog
            .iter()
            .filter(|m| m.enabled)
            .map(model_to_dto)
            .collect();
        Ok(ModelListDto { items })
    }

    /// `GET /v1/models/{id}`: 404 when unknown or disabled.
    ///
    /// # Errors
    /// 404 (model), 403/503 from the PEP, 500 on plugin failure.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        model_id: &str,
    ) -> Result<ModelDto, DomainError> {
        authz::model_permission(&self.deps.enforcer, ctx, authz::actions::READ).await?;
        let snapshot = self.deps.policy.current_snapshot(ctx.subject_id()).await?;
        snapshot
            .find_enabled(model_id)
            .map(model_to_dto)
            .ok_or(DomainError::NotFound {
                resource: resource_types::MODEL,
            })
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
