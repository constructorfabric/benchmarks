//! Model policy gateway: snapshots, user limits, usage publication.

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicySnapshot,
    PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::ClientHub;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::plugin_select::{PluginResolver, Resolved};

/// Resolved model of a chat (catalog entry plus its status).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Catalog entry.
    pub entry: mini_chat_sdk::ModelCatalogEntry,
}

/// Policy access through the selected `mini-chat-model-policy-plugin`.
pub struct PolicyGateway {
    resolver: PluginResolver<MiniChatModelPolicyPluginSpecV1, dyn MiniChatModelPolicyPluginClientV1>,
    fixed: Option<Arc<dyn MiniChatModelPolicyPluginClientV1>>,
}

/// Errors of usage publication.
#[derive(Debug)]
pub enum PublishOutcome {
    /// Retry later.
    Retry(String),
    /// Dead-letter.
    Reject(String),
}

impl PolicyGateway {
    /// Gateway resolving the plugin from types-registry.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self { resolver: PluginResolver::new(hub, vendor), fixed: None }
    }

    /// Gateway with a fixed plugin client (tests).
    #[must_use]
    pub fn fixed(hub: Arc<ClientHub>, client: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self { resolver: PluginResolver::new(hub, String::new()), fixed: Some(client) }
    }

    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        if let Some(c) = &self.fixed {
            return Ok(Arc::clone(c));
        }
        match self.resolver.resolve().await {
            Resolved::Ready(c) => Ok(c),
            Resolved::NotRegistered => {
                Err(DomainError::internal("no mini-chat model policy plugin registered"))
            }
            Resolved::ClientMissing(id) => Err(DomainError::internal(format!(
                "model policy plugin client not registered for '{id}'"
            ))),
            Resolved::Error(e) => Err(DomainError::internal(e)),
        }
    }

    /// Current snapshot for a user.
    ///
    /// # Errors
    /// Plugin resolution or plugin failure (internal).
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await?;
        let v = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        plugin
            .get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Snapshot of a specific version (settlement).
    ///
    /// # Errors
    /// Plugin resolution or plugin failure (internal).
    pub async fn snapshot_version(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await?;
        plugin
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// User limits under a version.
    ///
    /// # Errors
    /// Plugin resolution or plugin failure (internal).
    pub async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let plugin = self.plugin().await?;
        plugin
            .get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("user limits: {e}")))
    }

    /// Resolves the chat's model (without the enabled filter).
    ///
    /// # Errors
    /// `InvalidModel` when the model is not in the catalog; internal on plugin failure.
    pub async fn resolve_chat_model(
        &self,
        user_id: Uuid,
        model_id: &str,
    ) -> Result<(PolicySnapshot, ResolvedModel), DomainError> {
        let snapshot = self.current_snapshot(user_id).await?;
        let entry = snapshot.model(model_id).cloned().ok_or(DomainError::InvalidModel)?;
        Ok((snapshot, ResolvedModel { entry }))
    }

    /// Publishes a usage event.
    ///
    /// # Errors
    /// Retry on transient failures / missing plugin, Reject on permanent failures.
    pub async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishOutcome> {
        let plugin = self.plugin().await.map_err(|e| PublishOutcome::Retry(e.to_string()))?;
        plugin.publish_usage(event).await.map_err(|e| match e {
            PublishError::Transient(m) => PublishOutcome::Retry(m),
            PublishError::Permanent(m) => PublishOutcome::Reject(m),
        })
    }
}
