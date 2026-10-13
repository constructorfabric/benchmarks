//! Models service: model visibility and projection
//! (`cpt-cf-mini-chat-algo-gear-foundation-visible-models`) behind the PEP.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-models-api:p1

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

use crate::domain::authz::{AuthzService, ModelAction};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::PolicyCatalog;

fn project(entry: ModelCatalogEntry) -> ModelView {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-project-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-non-exposure
    // Exactly these seven fields: `provider_id`, `provider_model_id`,
    // `provider_display_name`, the credit multipliers, `policy_version`, the
    // token limits, `preference`, `estimation_budgets`, `general_config`, the
    // prompts and every routing field of the entry are dropped here.
    ModelView {
        model_id: entry.id,
        display_name: entry.display_name,
        tier: entry.tier,
        multiplier_display: entry.multiplier_display,
        description: Some(entry.description).filter(|d| !d.is_empty()),
        multimodal_capabilities: entry.multimodal_capabilities,
        context_window: entry.context_window,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-non-exposure
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-project-fields
}

/// Projection of an enabled catalog entry exposed by the Models API: exactly
/// these fields and nothing else.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelView {
    /// Stable model identifier (catalog `id`).
    pub model_id: String,
    /// UI display name.
    pub display_name: String,
    /// Rate-limit tier.
    pub tier: ModelTier,
    /// Informational multiplier label.
    pub multiplier_display: String,
    /// Help text; `None` when the catalog description is empty.
    pub description: Option<String>,
    /// Multimodal capability flags.
    pub multimodal_capabilities: Vec<String>,
    /// Maximum context window in tokens.
    pub context_window: u32,
}

/// Lists and reads the models visible to the caller.
pub struct ModelsService {
    authz: Arc<AuthzService>,
    policy: Arc<dyn PolicyCatalog>,
}

impl ModelsService {
    /// Creates the service.
    #[must_use]
    pub fn new(authz: Arc<AuthzService>, policy: Arc<dyn PolicyCatalog>) -> Self {
        Self { authz, policy }
    }

    /// The enabled catalog entries in catalog order (no authorization).
    ///
    /// # Errors
    ///
    /// [`DomainError::PolicyResolution`] when the plugin is not registered,
    /// cannot be resolved, fails or times out.
    pub async fn visible_models(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<ModelView>, DomainError> {
        let user_id = ctx.subject_id();
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-resolve-catalog-plugin
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-read-snapshot
        // The gateway resolves the plugin lazily; an unregistered plugin, a
        // resolution failure, a plugin failure and an elapsed bound all map to
        // the internal error.
        let version = self.policy.current_policy_version(user_id).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-resolve-catalog-plugin
        let snapshot = self
            .policy
            .policy_snapshot(user_id, version.policy_version)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-read-snapshot
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-filter-enabled
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-for-each-visible
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-return-projections
        Ok(snapshot
            .model_catalog
            .into_iter()
            .filter(|entry| entry.enabled)
            .map(project)
            .collect())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-return-projections
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-for-each-visible
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-visible-models:p1:inst-filter-enabled
    }

    /// `GET /v1/models`: authorizes `list` (decision only) and returns the
    /// visible models.
    ///
    /// # Errors
    ///
    /// `AuthzDenied`, `PdpUnavailable` or [`DomainError::PolicyResolution`].
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<Vec<ModelView>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-authorize-list
        self.authz.model_decision(ctx, ModelAction::List).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-authorize-list
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-compute-visible
        self.visible_models(ctx).await
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-compute-visible
    }

    /// `GET /v1/models/{id}`: authorizes `read` (decision only) and returns the
    /// visible model `id` (an opaque string).
    ///
    /// # Errors
    ///
    /// `NotFound { Model }` when `id` is absent or disabled, plus the errors of
    /// [`Self::list_models`].
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<ModelView, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-authorize-get
        self.authz.model_decision(ctx, ModelAction::Read).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-authorize-get
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-compute-visible-get
        let visible = self.visible_models(ctx).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-compute-visible-get
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-if-not-visible
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-return-404
        visible
            .into_iter()
            .find(|view| view.model_id == id)
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Model,
            })
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-return-404
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-if-not-visible
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
