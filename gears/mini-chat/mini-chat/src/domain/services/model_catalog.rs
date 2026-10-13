//! Model catalog resolution over the current policy snapshot (DESIGN §2.2
//! "Model Locked Per Chat", §4 "Model Catalog Configuration").

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use uuid::Uuid;

use toolkit_security::SecurityContext;

use crate::domain::authz::{ChatAuthz, actions};
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::gateways::model_policy::ModelPolicyGateway;

/// A catalog entry together with the snapshot it was resolved from.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub entry: ModelCatalogEntry,
    pub snapshot: Arc<PolicySnapshot>,
}

/// Reads the user's current policy snapshot on every call (no caching).
pub struct ModelCatalogService {
    policy: Arc<dyn ModelPolicyGateway>,
    authz: Arc<ChatAuthz>,
}

impl ModelCatalogService {
    #[must_use]
    pub fn new(policy: Arc<dyn ModelPolicyGateway>, authz: Arc<ChatAuthz>) -> Self {
        Self { policy, authz }
    }

    /// Model of an existing chat, without the `enabled` filter (a disabled model
    /// is downgraded by the quota cascade).
    ///
    /// # Errors
    /// `InvalidModel` when the model is no longer in the catalog.
    pub async fn resolve_chat_model(
        &self,
        user_id: Uuid,
        model_id: &str,
    ) -> DomainResult<ResolvedModel> {
        let snapshot = self.policy.current_snapshot(user_id).await?;
        let entry = snapshot
            .find(model_id)
            .cloned()
            .ok_or(DomainError::InvalidModel)?;
        Ok(ResolvedModel { entry, snapshot })
    }

    /// Model of a new chat: the requested enabled model, or the default (first
    /// enabled `is_default` entry, else the first enabled entry; tier ignored).
    ///
    /// # Errors
    /// `InvalidModel` when the requested model is unknown or disabled, or when no
    /// model is enabled.
    pub async fn resolve_for_create(
        &self,
        user_id: Uuid,
        requested: Option<&str>,
    ) -> DomainResult<ResolvedModel> {
        let snapshot = self.policy.current_snapshot(user_id).await?;
        let entry = match requested {
            Some(id) => snapshot.find(id).filter(|m| m.enabled),
            None => default_model(&snapshot),
        }
        .cloned()
        .ok_or(DomainError::InvalidModel)?;
        Ok(ResolvedModel { entry, snapshot })
    }

    /// Enabled models, in catalog order (Models API `list`).
    ///
    /// # Errors
    /// Authorization errors, policy plugin failures.
    pub async fn list(&self, ctx: &SecurityContext) -> DomainResult<Vec<ModelCatalogEntry>> {
        self.authz.model_permission(ctx, actions::LIST).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snapshot
            .model_catalog
            .iter()
            .filter(|m| m.enabled)
            .cloned()
            .collect())
    }

    /// One enabled model (Models API `read`).
    ///
    /// # Errors
    /// Authorization errors, `ModelNotFound` when the model is unknown or disabled.
    pub async fn get(&self, ctx: &SecurityContext, id: &str) -> DomainResult<ModelCatalogEntry> {
        self.authz.model_permission(ctx, actions::READ).await?;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        snapshot
            .find(id)
            .filter(|m| m.enabled)
            .cloned()
            .ok_or(DomainError::ModelNotFound)
    }
}

/// First enabled `is_default` entry, else the first enabled entry.
fn default_model(snapshot: &PolicySnapshot) -> Option<&ModelCatalogEntry> {
    let mut enabled = snapshot.model_catalog.iter().filter(|m| m.enabled);
    let first = enabled.clone().next();
    enabled
        .find(|m| m.preference.is_some_and(|p| p.is_default))
        .or(first)
}
