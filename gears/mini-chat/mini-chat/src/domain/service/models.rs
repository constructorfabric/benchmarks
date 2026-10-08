//! Models API (`GET /v1/models`, `GET /v1/models/{id}`).

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;

use super::Core;
use crate::domain::authz::{self, actions};
use crate::domain::error::{DomainError, Resource};

impl Core {
    /// Enabled catalog models in catalog order.
    ///
    /// # Errors
    /// PEP / policy errors.
    pub async fn list_models(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        authz::model_permission(&self.enforcer, ctx, actions::LIST).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snap
            .model_catalog
            .into_iter()
            .filter(|m| m.enabled)
            .collect())
    }

    /// One enabled model (404 when missing or disabled).
    ///
    /// # Errors
    /// 404 / PEP / policy errors.
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<ModelCatalogEntry, DomainError> {
        authz::model_permission(&self.enforcer, ctx, actions::READ).await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        snap.model_catalog
            .into_iter()
            .find(|m| m.enabled && m.id == id)
            .ok_or_else(|| DomainError::not_found(Resource::Model, id))
    }
}
