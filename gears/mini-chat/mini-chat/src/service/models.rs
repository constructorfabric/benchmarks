//! Models API (read-only, enabled catalog entries only).

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;

use super::AppState;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, DomainResult, Res};

impl AppState {
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn list_models(&self, ctx: &SecurityContext) -> DomainResult<Vec<ModelCatalogEntry>> {
        authz::model_permission(&self.enforcer, ctx, actions::LIST).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snap.enabled_models().cloned().collect())
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> DomainResult<ModelCatalogEntry> {
        authz::model_permission(&self.enforcer, ctx, actions::READ).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        snap.find_enabled(id)
            .cloned()
            .ok_or_else(|| DomainError::not_found(Res::Model, id))
    }
}
