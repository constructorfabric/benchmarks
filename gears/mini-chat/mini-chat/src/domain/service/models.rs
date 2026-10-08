//! Models API (read-only, enabled catalog entries only).

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;

use super::Service;
use crate::domain::error::{DomainError, DomainResult};

impl Service {
    /// `GET /v1/models`: enabled catalog entries.
    ///
    /// # Errors
    /// Authz errors, `PolicyResolution`.
    pub async fn list_models(&self, ctx: &SecurityContext) -> DomainResult<Vec<ModelCatalogEntry>> {
        self.authz.model_permission(ctx, "list").await?;
        let snapshot = self.snapshot(ctx.subject_id()).await?;
        Ok(snapshot
            .model_catalog
            .into_iter()
            .filter(|m| m.enabled)
            .collect())
    }

    /// `GET /v1/models/{id}`: 404 when missing or disabled.
    ///
    /// # Errors
    /// Authz errors, `ModelNotFound`, `PolicyResolution`.
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        model_id: &str,
    ) -> DomainResult<ModelCatalogEntry> {
        self.authz.model_permission(ctx, "read").await?;
        let snapshot = self.snapshot(ctx.subject_id()).await?;
        snapshot
            .model_catalog
            .into_iter()
            .find(|m| m.enabled && m.id == model_id)
            .ok_or_else(|| DomainError::ModelNotFound {
                id: model_id.to_owned(),
            })
    }
}
