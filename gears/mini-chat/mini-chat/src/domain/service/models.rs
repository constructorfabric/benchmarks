//! Models API (DESIGN §3.3 "Models API").

use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use toolkit_security::SecurityContext;

use super::{AppServices, policy};
use crate::domain::error::{DomainError, NotFoundKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelView {
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTier,
    pub multiplier_display: String,
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

impl From<&ModelCatalogEntry> for ModelView {
    fn from(m: &ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id.clone(),
            display_name: m.display_name.clone(),
            tier: m.tier,
            multiplier_display: m.multiplier_display.clone(),
            description: (!m.description.is_empty()).then(|| m.description.clone()),
            multimodal_capabilities: m.multimodal_capabilities.clone(),
            context_window: m.context_window,
        }
    }
}

impl AppServices {
    /// `GET /v1/models`: globally enabled catalog entries.
    ///
    /// # Errors
    /// Authorization or policy plugin failure.
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<Vec<ModelView>, DomainError> {
        self.authz.model_check(ctx, "list").await?;
        let snap = policy::current_snapshot(self.policy.as_ref(), ctx.subject_id()).await?;
        let mut items: Vec<&ModelCatalogEntry> = snap.model_catalog.iter().filter(|m| m.enabled).collect();
        items.sort_by_key(|m| m.preference.sort_order);
        Ok(items.into_iter().map(ModelView::from).collect())
    }

    /// `GET /v1/models/{id}`.
    ///
    /// # Errors
    /// `NotFound(Model)` for disabled or unknown models.
    pub async fn get_model(&self, ctx: &SecurityContext, id: &str) -> Result<ModelView, DomainError> {
        self.authz.model_check(ctx, "read").await?;
        let snap = policy::current_snapshot(self.policy.as_ref(), ctx.subject_id()).await?;
        snap.enabled_model(id)
            .map(ModelView::from)
            .ok_or(DomainError::NotFound(NotFoundKind::Model))
    }
}
