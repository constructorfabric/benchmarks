//! Model policy gateway: domain-facing port over the model policy plugin.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicySnapshot,
    PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::ClientHub;
use uuid::Uuid;

use super::plugin_select::PluginResolver;
use crate::domain::error::{DomainError, DomainResult};

/// Failure of `publish_usage`, driving the usage outbox handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// Not delivered; retry later (transient plugin error, plugin not available).
    Retry(String),
    /// Permanently rejected; do not retry.
    Reject(String),
}

/// Policy snapshots, user limits and usage settlement.
#[async_trait]
pub trait ModelPolicyGateway: Send + Sync {
    /// Snapshot of the user's current policy version.
    async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<Arc<PolicySnapshot>>;

    /// Snapshot of a specific policy version.
    async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<Arc<PolicySnapshot>>;

    /// Credit limits of the user for `version`.
    async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits>;

    /// Publish a usage settlement event (at-least-once).
    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome>;
}

fn plugin_err(e: impl std::fmt::Display) -> DomainError {
    DomainError::internal(format!("model policy plugin: {e}"))
}

async fn current_snapshot(
    client: &dyn MiniChatModelPolicyPluginClientV1,
    user_id: Uuid,
) -> DomainResult<Arc<PolicySnapshot>> {
    let info = client
        .get_current_policy_version(user_id)
        .await
        .map_err(plugin_err)?;
    snapshot(client, user_id, info.policy_version).await
}

async fn snapshot(
    client: &dyn MiniChatModelPolicyPluginClientV1,
    user_id: Uuid,
    version: u64,
) -> DomainResult<Arc<PolicySnapshot>> {
    client
        .get_policy_snapshot(user_id, version)
        .await
        .map(Arc::new)
        .map_err(plugin_err)
}

async fn user_limits(
    client: &dyn MiniChatModelPolicyPluginClientV1,
    user_id: Uuid,
    version: u64,
) -> DomainResult<UserLimits> {
    client
        .get_user_limits(user_id, version)
        .await
        .map_err(plugin_err)
}

async fn publish_usage(
    client: &dyn MiniChatModelPolicyPluginClientV1,
    ev: UsageEvent,
) -> Result<(), PublishOutcome> {
    client.publish_usage(ev).await.map_err(|e| match e {
        PublishError::Transient(_) => PublishOutcome::Retry(e.to_string()),
        PublishError::Permanent(_) => PublishOutcome::Reject(e.to_string()),
    })
}

/// Gateway resolving the plugin lazily by vendor through the types-registry.
pub struct PluginModelPolicyGateway {
    resolver: PluginResolver<MiniChatModelPolicyPluginSpecV1, dyn MiniChatModelPolicyPluginClientV1>,
}

impl PluginModelPolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            resolver: PluginResolver::new(
                hub,
                vendor,
                MiniChatModelPolicyPluginSpecV1::gts_type_id(),
            ),
        }
    }

    async fn client(&self) -> DomainResult<Arc<dyn MiniChatModelPolicyPluginClientV1>> {
        self.resolver.client().await.map_err(plugin_err)
    }
}

#[async_trait]
impl ModelPolicyGateway for PluginModelPolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<Arc<PolicySnapshot>> {
        current_snapshot(&*self.client().await?, user_id).await
    }

    async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<Arc<PolicySnapshot>> {
        snapshot(&*self.client().await?, user_id, version).await
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits> {
        user_limits(&*self.client().await?, user_id, version).await
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome> {
        let client = self
            .resolver
            .client()
            .await
            .map_err(|e| PublishOutcome::Retry(e.to_string()))?;
        publish_usage(&*client, ev).await
    }
}

/// Gateway over an in-process plugin client (bypasses the types-registry);
/// used by `mini_chat::testing`.
pub struct InProcessModelPolicyGateway {
    client: Arc<dyn MiniChatModelPolicyPluginClientV1>,
}

impl InProcessModelPolicyGateway {
    #[must_use]
    pub fn new(client: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ModelPolicyGateway for InProcessModelPolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<Arc<PolicySnapshot>> {
        current_snapshot(&*self.client, user_id).await
    }

    async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<Arc<PolicySnapshot>> {
        snapshot(&*self.client, user_id, version).await
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits> {
        user_limits(&*self.client, user_id, version).await
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome> {
        publish_usage(&*self.client, ev).await
    }
}
