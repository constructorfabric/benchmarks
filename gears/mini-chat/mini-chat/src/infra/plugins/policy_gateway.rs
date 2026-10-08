//! Model-policy plugin gateway.
//!
//! Resolves the `mini-chat-model-policy-plugin` instance of the configured
//! vendor through the types-registry (lazily, cached) and forwards calls to
//! its scoped client. There is no local snapshot cache (ADR-0008): every
//! caller asks the plugin.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicySnapshot,
    PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::PolicyProvider;

pub struct PolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl PolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    async fn resolve_instance(&self) -> Result<String, String> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| format!("types-registry client unavailable: {e}"))?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| format!("types-registry list failed: {e}"))?;
        choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| e.to_string())
    }

    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, String> {
        let id = self
            .selector
            .get_or_init(|| self.resolve_instance())
            .await?;
        if let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(&id))
        {
            return Ok(client);
        }
        self.selector.reset().await;
        Err(format!(
            "model policy plugin client not registered for '{id}'"
        ))
    }
}

fn policy_err(detail: impl Into<String>) -> DomainError {
    let detail = detail.into();
    tracing::error!(%detail, "mini-chat: model policy resolution failed");
    DomainError::PolicyResolution { detail }
}

#[async_trait]
impl PolicyProvider for PolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await.map_err(policy_err)?;
        let version = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| policy_err(e.to_string()))?;
        plugin
            .get_policy_snapshot(user_id, version.policy_version)
            .await
            .map_err(|e| policy_err(e.to_string()))
    }

    async fn snapshot(&self, user_id: Uuid, version: u64) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await.map_err(policy_err)?;
        plugin
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| policy_err(e.to_string()))
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let plugin = self.plugin().await.map_err(policy_err)?;
        plugin
            .get_user_limits(user_id, version)
            .await
            .map_err(|e| policy_err(e.to_string()))
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        let plugin = self.plugin().await.map_err(PublishError::Transient)?;
        plugin.publish_usage(event).await
    }
}
