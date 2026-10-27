//! Plugin gateways: resolve plugin instances via types-registry and obtain
//! their clients from the `ClientHub`.

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Errors of plugin resolution.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// No plugin instance is registered for the vendor.
    #[error("no plugin registered")]
    NoPlugin,
    /// The instance resolved but its client is missing from the `ClientHub`.
    #[error("plugin client missing")]
    ClientMissing,
    /// Types-registry failure.
    #[error("types registry unavailable: {0}")]
    RegistryUnavailable(String),
    /// The policy plugin returned an error.
    #[error("policy plugin error: {0}")]
    Policy(#[from] mini_chat_sdk::ModelPolicyError),
    /// `Usage` publication failed.
    #[error("publish error: {0}")]
    Publish(#[from] mini_chat_sdk::PublishError),
    /// Audit delivery failed.
    #[error("audit error: {0}")]
    Audit(#[from] mini_chat_sdk::AuditError),
}

/// The model policy port (snapshot, limits, usage publication).
#[async_trait::async_trait]
pub trait PolicyPort: Send + Sync {
    /// Current policy version for the user.
    async fn current_policy_version(&self, user_id: uuid::Uuid) -> Result<i64, GatewayError>;
    /// Policy snapshot by version.
    async fn policy_snapshot(
        &self,
        user_id: uuid::Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, GatewayError>;
    /// User limits under a policy version.
    async fn user_limits(
        &self,
        user_id: uuid::Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, GatewayError>;
    /// Publish a usage event.
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), GatewayError>;
}

/// The audit delivery port.
#[async_trait::async_trait]
pub trait AuditPort: Send + Sync {
    /// Resolve the current audit target (re-resolved per delivery).
    async fn target(&self) -> AuditTarget;
}

/// Model policy gateway: policy snapshots, user limits, usage publication.
pub struct ModelPolicyGateway {
    hub: Arc<ClientHub>,
    registry: Arc<dyn TypesRegistryClient>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl ModelPolicyGateway {
    /// Create the gateway.
    #[must_use]
    pub fn new(
        hub: Arc<ClientHub>,
        registry: Arc<dyn TypesRegistryClient>,
        vendor: String,
    ) -> Self {
        Self {
            hub,
            registry,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    async fn resolve_instance_id(&self) -> Result<String, GatewayError> {
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
        let instances = self
            .registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| GatewayError::RegistryUnavailable(e.to_string()))?;
        let id = choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| {
            tracing::warn!("mini-chat: model policy plugin selection failed: {e}");
            GatewayError::NoPlugin
        })?;
        Ok(id)
    }

    async fn client(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, GatewayError> {
        let instance_id = self
            .selector
            .get_or_init(|| self.resolve_instance_id())
            .await?;
        let scope = ClientScope::gts_id(instance_id.as_ref());
        self.hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&scope)
            .ok_or(GatewayError::ClientMissing)
    }

}

#[async_trait::async_trait]
impl PolicyPort for ModelPolicyGateway {
    async fn current_policy_version(&self, user_id: uuid::Uuid) -> Result<i64, GatewayError> {
        self.client().await?.get_current_policy_version(user_id).await.map_err(Into::into)
    }

    async fn policy_snapshot(
        &self,
        user_id: uuid::Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, GatewayError> {
        self.client()
            .await?
            .get_policy_snapshot(user_id, policy_version)
            .await
            .map_err(Into::into)
    }

    async fn user_limits(
        &self,
        user_id: uuid::Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, GatewayError> {
        self.client()
            .await?
            .get_user_limits(user_id, policy_version)
            .await
            .map_err(Into::into)
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), GatewayError> {
        self.client().await?.publish_usage(event).await.map_err(Into::into)
    }
}

/// Audit plugin resolution outcome.
pub enum AuditTarget {
    /// The plugin client to deliver to.
    Client(Arc<dyn MiniChatAuditPluginClientV1>),
    /// No plugin is registered; drop the event.
    NoPlugin,
    /// The instance resolved but the client is missing; retry later.
    Retry,
    /// The types-registry is unavailable; retry later.
    Unavailable,
}

/// Audit gateway: resolves the audit plugin per delivery (the "no plugin"
/// result is not cached; a found instance id is cached and reset when the
/// client is missing).
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    registry: Arc<dyn TypesRegistryClient>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl AuditGateway {
    /// Create the gateway.
    #[must_use]
    pub fn new(
        hub: Arc<ClientHub>,
        registry: Arc<dyn TypesRegistryClient>,
        vendor: String,
    ) -> Self {
        Self {
            hub,
            registry,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    async fn resolve_instance_id(&self) -> Result<String, GatewayError> {
        let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
        let instances = self
            .registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| GatewayError::RegistryUnavailable(e.to_string()))?;
        let id = choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| {
            tracing::debug!("mini-chat: audit plugin selection failed: {e}");
            GatewayError::NoPlugin
        })?;
        Ok(id)
    }

    async fn target_impl(&self) -> AuditTarget {
        let instance_id = match self
            .selector
            .get_or_init(|| self.resolve_instance_id())
            .await
        {
            Ok(id) => id,
            Err(GatewayError::NoPlugin) => {
                tracing::warn!("mini-chat: no audit plugin registered; dropping audit events");
                return AuditTarget::NoPlugin;
            }
            Err(_) => return AuditTarget::Unavailable,
        };
        let scope = ClientScope::gts_id(instance_id.as_ref());
        match self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&scope)
        {
            Some(client) => AuditTarget::Client(client),
            None => {
                // Resolved instance without a client: reset the cached
                // instance id so a later delivery re-resolves.
                self.selector.reset().await;
                AuditTarget::Retry
            }
        }
    }
}

#[async_trait::async_trait]
impl AuditPort for AuditGateway {
    async fn target(&self) -> AuditTarget {
        self.target_impl().await
    }
}
