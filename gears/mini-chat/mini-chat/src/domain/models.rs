//! Models service (DESIGN 3.3 "Models API"): read-only view of the policy
//! catalog visible to the caller.
//!
//! Per request: PDP decision first (a denied or unreachable PDP never touches
//! the plugin), then the current snapshot from the model-policy plugin (no
//! caching, ADR-0008), filtered to `enabled` entries. Only the public
//! projection of an entry leaves the service: provider ids, credit
//! multipliers, token limits, the default flag and the enabled flag are never
//! exposed.

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use toolkit_security::SecurityContext;

use crate::domain::authz::{actions, authorize_decision, model_resource};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::ModelPolicyPort;

/// Public tier of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTierView {
    /// Standard tier.
    Standard,
    /// Premium tier.
    Premium,
}

/// Public projection of a catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelView {
    /// Public model identifier.
    pub model_id: String,
    /// Display name.
    pub display_name: String,
    /// Tier.
    pub tier: ModelTierView,
    /// Human readable credit multiplier.
    pub multiplier_display: String,
    /// Description; absent when the catalog has none.
    pub description: Option<String>,
    /// Multimodal capabilities.
    pub multimodal_capabilities: Vec<String>,
    /// Context window in tokens.
    pub context_window: u32,
}

impl From<&ModelCatalogEntry> for ModelView {
    fn from(entry: &ModelCatalogEntry) -> Self {
        Self {
            model_id: entry.id.clone(),
            display_name: entry.display_name.clone(),
            tier: match entry.tier {
                ModelTier::Premium => ModelTierView::Premium,
                ModelTier::Standard => ModelTierView::Standard,
            },
            multiplier_display: entry.multiplier_display.clone(),
            description: (!entry.description.is_empty()).then(|| entry.description.clone()),
            multimodal_capabilities: entry.multimodal_capabilities.clone(),
            context_window: entry.context_window,
        }
    }
}

/// Lists and fetches visible models.
pub struct ModelsService {
    policy: Arc<dyn ModelPolicyPort>,
    enforcer: PolicyEnforcer,
}

impl ModelsService {
    /// Creates the service.
    #[must_use]
    pub fn new<P: ModelPolicyPort + 'static>(policy: Arc<P>, enforcer: PolicyEnforcer) -> Self {
        Self { policy, enforcer }
    }

    /// All enabled models of the current policy snapshot.
    ///
    /// # Errors
    /// `AccessDenied` / `ServiceUnavailable` from the PDP, `Internal` when the
    /// policy plugin cannot be resolved or fails.
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<Vec<ModelView>, DomainError> {
        authorize_decision(&self.enforcer, ctx, &model_resource(), actions::LIST).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snapshot
            .model_catalog
            .iter()
            .filter(|entry| entry.enabled)
            .map(ModelView::from)
            .collect())
    }

    /// One enabled model; unknown and disabled ids are indistinguishable.
    ///
    /// # Errors
    /// `NotFound` for an unknown or disabled model, otherwise as
    /// [`Self::list_models`].
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<ModelView, DomainError> {
        authorize_decision(&self.enforcer, ctx, &model_resource(), actions::READ).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.enabled && entry.id == id)
            .map(ModelView::from)
            .ok_or_else(|| DomainError::NotFound {
                resource: ResourceKind::Model,
                id: id.to_owned(),
            })
    }
}
