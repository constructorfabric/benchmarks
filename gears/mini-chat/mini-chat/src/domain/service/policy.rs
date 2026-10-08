//! Model policy gateway: resolves the `mini-chat-model-policy-plugin` instance
//! through types-registry (no snapshot cache, ADR-0008).

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

/// Source of the model policy plugin.
#[async_trait]
pub trait PolicySource: Send + Sync {
    /// # Errors
    /// The plugin cannot be resolved.
    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError>;
}

/// Fixed plugin (tests, embedded use).
pub struct FixedPolicySource(pub Arc<dyn MiniChatModelPolicyPluginClientV1>);

#[async_trait]
impl PolicySource for FixedPolicySource {
    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        Ok(Arc::clone(&self.0))
    }
}

/// types-registry backed resolution by vendor and priority.
pub struct RegistryPolicySource {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl RegistryPolicySource {
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
            .map_err(|e| DomainError::internal(format!("types-registry unavailable: {e}")))?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| DomainError::internal(format!("policy plugin lookup failed: {e}")))?;
        choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| DomainError::internal(format!("policy plugin not available: {e}")))
    }
}

#[async_trait]
impl PolicySource for RegistryPolicySource {
    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        let id = self.selector.get_or_init(|| self.resolve_instance()).await?;
        if let Some(c) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(id.as_ref()))
        {
            return Ok(c);
        }
        self.selector.reset().await;
        Err(DomainError::internal("policy plugin client not registered yet"))
    }
}

fn plugin_err(e: impl std::fmt::Display) -> DomainError {
    DomainError::internal(format!("model policy plugin failure: {e}"))
}

/// Current policy snapshot for a user.
///
/// # Errors
/// Plugin failure (500).
pub async fn current_snapshot(src: &dyn PolicySource, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
    let p = src.plugin().await?;
    let v = p.get_current_policy_version(user_id).await.map_err(plugin_err)?;
    p.get_policy_snapshot(user_id, v).await.map_err(plugin_err)
}

/// Snapshot of a given version.
///
/// # Errors
/// Plugin failure (500).
pub async fn snapshot_version(
    src: &dyn PolicySource,
    user_id: Uuid,
    version: u64,
) -> Result<PolicySnapshot, DomainError> {
    let p = src.plugin().await?;
    p.get_policy_snapshot(user_id, version).await.map_err(plugin_err)
}

/// User limits under a policy version.
///
/// # Errors
/// Plugin failure (500).
pub async fn user_limits(src: &dyn PolicySource, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
    let p = src.plugin().await?;
    p.get_user_limits(user_id, version).await.map_err(plugin_err)
}
