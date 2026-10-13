//! Model resolution against the current policy snapshot (DESIGN "Model
//! Catalog Configuration", "Models API" visibility algorithm).

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::PolicyPort;

/// Resolves catalog models for a user (no local snapshot cache, ADR-0008).
pub struct ModelResolver {
    policy: Arc<dyn PolicyPort>,
}

impl ModelResolver {
    #[must_use]
    pub fn new(policy: Arc<dyn PolicyPort>) -> Self {
        Self { policy }
    }

    /// Current policy snapshot of the user.
    ///
    /// # Errors
    /// Policy plugin failures (`PluginUnavailable` / `Internal`).
    pub async fn snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        self.policy.current_snapshot(user_id).await
    }

    /// Catalog entry by id (enabled or not).
    #[must_use]
    pub fn find<'a>(snap: &'a PolicySnapshot, id: &str) -> Option<&'a ModelCatalogEntry> {
        snap.model_catalog.iter().find(|m| m.id == id)
    }

    /// Model for a new chat: the requested enabled entry, or by default the
    /// first enabled entry with `preference.is_default`, else the first
    /// enabled entry.
    ///
    /// # Errors
    /// `InvalidModel` when the requested model is unknown/disabled or no
    /// enabled model exists; policy plugin failures.
    pub async fn resolve_for_new_chat(
        &self,
        user_id: Uuid,
        requested: Option<&str>,
    ) -> Result<ModelCatalogEntry, DomainError> {
        let snap = self.snapshot(user_id).await?;
        let mut enabled = snap.model_catalog.iter().filter(|m| m.enabled);
        let chosen = if let Some(id) = requested {
            enabled.find(|m| m.id == id)
        } else {
            let enabled: Vec<&ModelCatalogEntry> = enabled.collect();
            enabled
                .iter()
                .find(|m| m.preference.is_some_and(|p| p.is_default))
                .or_else(|| enabled.first())
                .copied()
        };
        chosen.cloned().ok_or(DomainError::InvalidModel)
    }

    /// Model of an existing chat (no enabled filter; a disabled model is
    /// downgraded later by the quota cascade).
    ///
    /// # Errors
    /// `InvalidModel` when the model is no longer in the catalog; policy
    /// plugin failures.
    pub async fn resolve_chat_model(
        &self,
        user_id: Uuid,
        model_id: &str,
    ) -> Result<(Arc<PolicySnapshot>, ModelCatalogEntry), DomainError> {
        let snap = self.snapshot(user_id).await?;
        let entry = Self::find(&snap, model_id)
            .cloned()
            .ok_or(DomainError::InvalidModel)?;
        Ok((snap, entry))
    }

    /// Globally enabled models, in catalog order.
    ///
    /// # Errors
    /// Policy plugin failures.
    pub async fn visible_models(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        let snap = self.snapshot(user_id).await?;
        Ok(snap
            .model_catalog
            .iter()
            .filter(|m| m.enabled)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
#[path = "model_resolver_tests.rs"]
mod tests;
