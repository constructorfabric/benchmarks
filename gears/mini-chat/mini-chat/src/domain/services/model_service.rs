//! Model catalog service (DESIGN section 3.3, Models API).
//!
//! The catalog is read from the model policy plugin on every call (no local
//! snapshot cache). Only `enabled` entries are visible through the API.

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{AuthzPort, PolicyProvider};

pub struct ModelService {
    authz: Arc<dyn AuthzPort>,
    policy: Arc<dyn PolicyProvider>,
}

impl ModelService {
    #[must_use]
    pub fn new(authz: Arc<dyn AuthzPort>, policy: Arc<dyn PolicyProvider>) -> Self {
        Self { authz, policy }
    }

    /// Enabled catalog entries in catalog order (`model_access(ctx, "list")`).
    ///
    /// # Errors
    /// Authorization failure or policy plugin failure.
    pub async fn list(&self, ctx: &SecurityContext) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.authz.model_access(ctx, "list").await?;
        let snapshot = self.policy.current(ctx.subject_id()).await?;
        Ok(snapshot.enabled().cloned().collect())
    }

    /// One enabled catalog entry (`model_access(ctx, "read")`). A disabled or
    /// unknown model is `NotFound` (resource `model`).
    ///
    /// # Errors
    /// Authorization failure, `NotFound`, or policy plugin failure.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<ModelCatalogEntry, DomainError> {
        self.authz.model_access(ctx, "read").await?;
        let snapshot = self.policy.current(ctx.subject_id()).await?;
        snapshot
            .find(id)
            .filter(|m| m.enabled)
            .cloned()
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Model,
            })
    }

    /// The user's current snapshot and the entry for `model_id`. With
    /// `enabled_only` a disabled entry is `InvalidModel`; otherwise only a
    /// model missing from the catalog is.
    ///
    /// # Errors
    /// `InvalidModel`, or policy plugin failure.
    pub async fn resolve_for_chat(
        &self,
        user_id: Uuid,
        model_id: &str,
        enabled_only: bool,
    ) -> Result<(Arc<PolicySnapshot>, ModelCatalogEntry), DomainError> {
        let snapshot = self.policy.current(user_id).await?;
        let entry = snapshot
            .find(model_id)
            .filter(|m| m.enabled || !enabled_only)
            .cloned()
            .ok_or(DomainError::InvalidModel)?;
        Ok((snapshot, entry))
    }

    /// The model a new chat gets when the request names none: the first
    /// enabled `is_default` entry, else the first enabled entry.
    ///
    /// # Errors
    /// `InvalidModel` when no model is enabled, or policy plugin failure.
    pub async fn default_for_chat(&self, user_id: Uuid) -> Result<ModelCatalogEntry, DomainError> {
        let snapshot = self.policy.current(user_id).await?;
        snapshot
            .default_model()
            .cloned()
            .ok_or(DomainError::InvalidModel)
    }
}

#[cfg(test)]
#[path = "model_service_tests.rs"]
mod model_service_tests;
