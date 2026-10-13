//! Models API service and model resolution helpers (DESIGN section 3.3
//! "Models API", "Model Catalog Configuration").
//!
//! The canonical catalog comes from the model-policy plugin. Every call reads
//! the current policy version and its snapshot (no local snapshot cache,
//! ADR-0008).
//!
//! - **Visibility** — a model is visible when `enabled = true`; the list keeps
//!   the catalog order and a hidden or unknown model is a `404 not_found` on
//!   the model resource type, so disabled and unknown models are
//!   indistinguishable.
//! - **Default model** — the first enabled entry with `preference.is_default`,
//!   else the first enabled entry; tier is not considered.
//! - **New chat** — a requested model must exist and be enabled, otherwise
//!   `400 INVALID_MODEL`.
//! - **Existing chat** — the chat's model is looked up without the `enabled`
//!   filter; a model that left the catalog is `400 INVALID_MODEL`.
//!
//! Plugin failures are internal errors (500); their text stays server-side.

use std::sync::Arc;

use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, PolicySnapshot};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::authz::{Action, AuthzService};
use crate::domain::error::{DomainError, InvalidArgumentReason, ResourceKind};

/// A catalog model resolved against one policy version.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    /// Policy version the snapshot was read at.
    pub policy_version: u64,
    /// The resolved catalog entry.
    pub model: ModelCatalogEntry,
    /// The full snapshot of `policy_version` (kill switches, other tiers).
    pub snapshot: PolicySnapshot,
}

/// Models API domain service.
pub struct ModelsService {
    policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    authz: Arc<AuthzService>,
}

impl std::fmt::Debug for ModelsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelsService")
            .field("authz", &self.authz)
            .finish_non_exhaustive()
    }
}

impl ModelsService {
    /// Builds the service over the resolved model-policy plugin client and the
    /// gear PEP.
    #[must_use]
    pub fn new(
        policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
        authz: Arc<AuthzService>,
    ) -> Self {
        Self { policy, authz }
    }

    /// Reads the user's current policy version and its snapshot from the
    /// plugin.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the plugin fails either call.
    pub async fn current_policy(
        &self,
        user_id: Uuid,
    ) -> Result<(u64, PolicySnapshot), DomainError> {
        let version = self
            .policy
            .get_current_policy_version(user_id)
            .await
            .map_err(|err| plugin_error("get_current_policy_version", &err))?;
        let snapshot = self
            .policy
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|err| plugin_error("get_policy_snapshot", &err))?;
        Ok((version, snapshot))
    }

    /// `GET /v1/models`: the enabled catalog models in catalog order.
    ///
    /// # Errors
    ///
    /// PEP errors of [`AuthzService::authorize_model`] (`list`) and
    /// [`DomainError::Internal`] on plugin failure.
    pub async fn list_visible(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.authz.authorize_model(ctx, Action::List).await?;
        let (_, snapshot) = self.current_policy(ctx.subject_id()).await?;
        Ok(snapshot
            .model_catalog
            .into_iter()
            .filter(|entry| entry.enabled)
            .collect())
    }

    /// `GET /v1/models/{id}`: the enabled catalog model `id`.
    ///
    /// # Errors
    ///
    /// PEP errors of [`AuthzService::authorize_model`] (`read`),
    /// [`DomainError::NotFound`] (model) when `id` is unknown or disabled and
    /// [`DomainError::Internal`] on plugin failure.
    pub async fn get_visible(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<ModelCatalogEntry, DomainError> {
        self.authz.authorize_model(ctx, Action::Read).await?;
        let (_, snapshot) = self.current_policy(ctx.subject_id()).await?;
        snapshot
            .model_catalog
            .into_iter()
            .find(|entry| entry.enabled && entry.id == id)
            .ok_or_else(|| DomainError::not_found(ResourceKind::Model, id))
    }

    /// Resolves the model of a new chat: the requested model when it exists
    /// and is enabled, else (nothing requested) the default model.
    ///
    /// # Errors
    ///
    /// `400 INVALID_MODEL` when the requested model is unknown or disabled, or
    /// when nothing is requested and the catalog has no enabled model;
    /// [`DomainError::Internal`] on plugin failure.
    pub async fn resolve_for_new_chat(
        &self,
        ctx: &SecurityContext,
        requested: Option<&str>,
    ) -> Result<ResolvedModel, DomainError> {
        let (policy_version, snapshot) = self.current_policy(ctx.subject_id()).await?;
        let model = match requested {
            Some(id) => snapshot
                .model_catalog
                .iter()
                .find(|entry| entry.enabled && entry.id == id),
            None => resolve_default_model(&snapshot),
        }
        .cloned()
        .ok_or_else(invalid_model)?;
        Ok(ResolvedModel {
            policy_version,
            model,
            snapshot,
        })
    }

    /// Resolves the stored model of an existing chat in the user's current
    /// policy, regardless of `enabled`.
    ///
    /// # Errors
    ///
    /// `400 INVALID_MODEL` when the model is no longer in the catalog;
    /// [`DomainError::Internal`] on plugin failure.
    pub async fn resolve_chat_model(
        &self,
        user_id: Uuid,
        model_id: &str,
    ) -> Result<ResolvedModel, DomainError> {
        let (policy_version, snapshot) = self.current_policy(user_id).await?;
        let model = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == model_id)
            .cloned()
            .ok_or_else(invalid_model)?;
        Ok(ResolvedModel {
            policy_version,
            model,
            snapshot,
        })
    }
}

/// Default model of new chats: the first enabled entry with
/// `preference.is_default = true`, else the first enabled entry (tier is not
/// considered); `None` when no entry is enabled.
#[must_use]
pub fn resolve_default_model(snapshot: &PolicySnapshot) -> Option<&ModelCatalogEntry> {
    let catalog = &snapshot.model_catalog;
    catalog
        .iter()
        .find(|entry| entry.enabled && entry.preference.is_some_and(|p| p.is_default))
        .or_else(|| catalog.iter().find(|entry| entry.enabled))
}

const fn invalid_model() -> DomainError {
    DomainError::invalid_argument(InvalidArgumentReason::InvalidModel)
}

/// The diagnostic is logged by the platform error middleware, never serialized.
fn plugin_error(call: &str, err: &mini_chat_sdk::MiniChatModelPolicyPluginError) -> DomainError {
    DomainError::internal(format!("model policy plugin {call} failed: {err}"))
}

#[cfg(test)]
#[path = "models_service_tests.rs"]
pub(crate) mod tests;
