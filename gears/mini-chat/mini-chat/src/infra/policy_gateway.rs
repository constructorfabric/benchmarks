//! Model policy gateway: resolves the `mini-chat-model-policy-plugin`
//! instance through the types registry and reads policy snapshots and user
//! limits from it (no local cache, ADR-0008).

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicySnapshot, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Port used by the domain to reach the model-policy plugin.
#[async_trait]
pub trait PolicyProvider: Send + Sync {
    /// The selected plugin client.
    ///
    /// # Errors
    /// Plugin not registered / registry failure (internal).
    async fn client(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError>;

    /// Current snapshot for the user.
    ///
    /// # Errors
    /// Plugin failures (internal).
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let c = self.client().await?;
        let v = c
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::Internal(format!("policy plugin: {e}")))?;
        c.get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::Internal(format!("policy plugin: {e}")))
    }

    /// Snapshot of a given version (settlement).
    ///
    /// # Errors
    /// Plugin failures (internal).
    async fn snapshot(&self, user_id: Uuid, version: u64) -> Result<PolicySnapshot, DomainError> {
        let c = self.client().await?;
        c.get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::Internal(format!("policy plugin: {e}")))
    }

    /// User limits under `version`.
    ///
    /// # Errors
    /// Plugin failures (internal).
    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let c = self.client().await?;
        c.get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::Internal(format!("policy plugin: {e}")))
    }
}

/// Types-registry backed provider.
pub struct GtsPolicyProvider {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl GtsPolicyProvider {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    async fn resolve_instance(&self) -> Result<String, DomainError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::Internal(format!("types registry unavailable: {e}")))?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| DomainError::Internal(format!("types registry list failed: {e}")))?;
        choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| DomainError::Internal(format!("model policy plugin: {e}")))
    }
}

#[async_trait]
impl PolicyProvider for GtsPolicyProvider {
    async fn client(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        let id = self
            .selector
            .get_or_init(|| self.resolve_instance())
            .await?;
        if let Some(c) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(&id))
        {
            return Ok(c);
        }
        self.selector.reset().await;
        Err(DomainError::Internal(format!(
            "model policy plugin client not registered for '{id}'"
        )))
    }
}

/// Fixed client (tests and single-plugin wiring).
pub struct StaticPolicyProvider(pub Arc<dyn MiniChatModelPolicyPluginClientV1>);

#[async_trait]
impl PolicyProvider for StaticPolicyProvider {
    async fn client(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        Ok(Arc::clone(&self.0))
    }
}
