//! Read-only Models API: only enabled catalog entries are visible.

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;

use crate::domain::app::AppServices;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Resource};
use crate::domain::policy::enabled_models;

impl AppServices {
    /// # Errors
    /// Authorization and policy plugin errors.
    pub async fn list_models(&self, ctx: &SecurityContext) -> DomainResult<Vec<ModelCatalogEntry>> {
        authz::model_permission(&self.enforcer, ctx, actions::LIST).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(enabled_models(&snapshot).cloned().collect())
    }

    /// # Errors
    /// `NotFound(Model)` for unknown or disabled models.
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> DomainResult<ModelCatalogEntry> {
        authz::model_permission(&self.enforcer, ctx, actions::READ).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        enabled_models(&snapshot)
            .find(|m| m.id == id)
            .cloned()
            .ok_or(DomainError::NotFound(Resource::Model))
    }
}
