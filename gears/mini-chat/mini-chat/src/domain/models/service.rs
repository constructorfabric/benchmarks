//! Models API service: the read-only catalog visible to the caller.

use std::sync::Arc;

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

use super::catalog::{enabled_models, find_enabled_model};
use crate::domain::authz::{Pep, actions};
use crate::domain::error::DomainError;
use crate::domain::ports::ModelPolicyGateway;

/// Lists and reads the globally enabled models of the current policy. Authorization is a pure
/// permission check (no constraints).
#[domain_model]
pub struct ModelsService {
    pep: Pep,
    policy: Arc<dyn ModelPolicyGateway>,
}

impl ModelsService {
    /// Creates the service.
    #[must_use]
    pub fn new(pep: Pep, policy: Arc<dyn ModelPolicyGateway>) -> Self {
        Self { pep, policy }
    }

    /// The globally enabled models, in catalog order.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable` or `PolicyUnavailable`.
    pub async fn list_models(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.pep.require_model_access(ctx, actions::LIST).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(enabled_models(&snapshot.model_catalog).cloned().collect())
    }

    /// One globally enabled model.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `PolicyUnavailable` or `ModelNotFound` when the model is
    /// disabled or not in the catalog.
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        model_id: &str,
    ) -> Result<ModelCatalogEntry, DomainError> {
        self.pep.require_model_access(ctx, actions::READ).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        find_enabled_model(&snapshot.model_catalog, model_id)
            .cloned()
            .ok_or_else(|| DomainError::ModelNotFound {
                model_id: model_id.to_owned(),
            })
    }
}
