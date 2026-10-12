//! Model-policy gateway (DESIGN 3.2 "Model policy gateway").
//!
//! Resolves the `mini-chat-model-policy-plugin` instance of the configured
//! vendor through types-registry (lazily, cached on success) and exposes the
//! policy snapshot and per-user limits. Every resolution or plugin failure is
//! an opaque `DomainError::Internal`: the plugin text is logged, never
//! returned (DESIGN 3.3 error table: "provider or policy resolution failure").

use std::sync::Arc;
use std::time::Duration;

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
use crate::domain::ports::ModelPolicyPort;

/// Plugin call timeout of `publish_usage` (a timeout is a transient failure).
pub const USAGE_PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of one usage delivery attempt; the outbox handler maps it to ack /
/// retry / reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageDeliveryOutcome {
    /// The plugin accepted the event.
    Delivered,
    /// Delivery failed transiently (the plugin cannot be resolved, a transient
    /// error, a timeout); the event is retried.
    Retry,
    /// The plugin refused the event for good; it is dead-lettered.
    Reject(String),
}

enum Source {
    /// A fixed client (no registry lookup).
    Fixed(Arc<dyn MiniChatModelPolicyPluginClientV1>),
    /// Resolved through types-registry and the `ClientHub`.
    Registry {
        hub: Arc<ClientHub>,
        vendor: String,
        selector: GtsPluginSelector,
    },
}

/// Gateway to the model-policy plugin.
pub struct ModelPolicyGateway {
    source: Source,
}

fn log_delivery(level_error: bool, what: &str, reason: &str) {
    if level_error {
        tracing::error!(%reason, "model policy plugin: {what}");
    } else {
        tracing::warn!(%reason, "model policy plugin: {what}");
    }
}

/// Maps the result of one `publish_usage` call (or its timeout) to the outcome
/// the outbox handler acts on.
fn delivery_outcome(
    result: Result<Result<(), PublishError>, tokio::time::error::Elapsed>,
) -> UsageDeliveryOutcome {
    match result {
        Err(_) => {
            log_delivery(false, "publish_usage timed out", "timeout");
            UsageDeliveryOutcome::Retry
        }
        Ok(Ok(())) => UsageDeliveryOutcome::Delivered,
        Ok(Err(PublishError::Transient(reason))) => {
            log_delivery(false, "transient publish_usage failure", &reason);
            UsageDeliveryOutcome::Retry
        }
        Ok(Err(PublishError::Permanent(reason))) => {
            log_delivery(true, "publish_usage rejected the event", &reason);
            UsageDeliveryOutcome::Reject(reason)
        }
    }
}

fn internal(message: &str, cause: Option<&dyn std::fmt::Display>) -> DomainError {
    let cause = cause.map(ToString::to_string).unwrap_or_default();
    tracing::warn!(error = %cause, "{message}");
    DomainError::internal(message)
}

impl ModelPolicyGateway {
    /// Gateway that resolves the plugin of `vendor` lazily through the types-registry.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        Self {
            source: Source::Registry {
                hub,
                vendor: vendor.into(),
                selector: GtsPluginSelector::new(),
            },
        }
    }

    /// Gateway bound to a fixed plugin client.
    #[must_use]
    pub fn from_client(client: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self {
            source: Source::Fixed(client),
        }
    }

    async fn resolve(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        let (hub, vendor, selector) = match &self.source {
            Source::Fixed(client) => return Ok(Arc::clone(client)),
            Source::Registry {
                hub,
                vendor,
                selector,
            } => (hub, vendor, selector),
        };
        let instance_id = selector
            .get_or_init(|| async {
                let registry = hub
                    .get::<dyn TypesRegistryClient>()
                    .map_err(|e| internal("types-registry client unavailable", Some(&e)))?;
                let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
                let instances = registry
                    .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
                    .await
                    .map_err(|e| internal("types-registry listing failed", Some(&e)))?;
                choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
                    vendor,
                    instances.iter().map(|e| (e.id.as_ref(), &e.object)),
                )
                .map_err(|e| internal("no model policy plugin for the configured vendor", Some(&e)))
            })
            .await?;
        hub.try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(
            instance_id.as_ref(),
        ))
        .ok_or_else(|| internal("model policy plugin client is not registered", None))
    }

    /// Current policy snapshot for the user: the current version first, then
    /// the snapshot of that version. Never cached.
    ///
    /// # Errors
    /// `Internal` when the plugin cannot be resolved or fails.
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.resolve().await?;
        let version = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| internal("model policy plugin: policy version failed", Some(&e)))?;
        plugin
            .get_policy_snapshot(user_id, version.policy_version)
            .await
            .map_err(|e| internal("model policy plugin: policy snapshot failed", Some(&e)))
    }

    /// The current policy version for the user.
    ///
    /// # Errors
    /// `Internal` when the plugin cannot be resolved or fails.
    pub async fn current_policy_version(&self, user_id: Uuid) -> Result<u64, DomainError> {
        let plugin = self.resolve().await?;
        plugin
            .get_current_policy_version(user_id)
            .await
            .map(|version| version.policy_version)
            .map_err(|e| internal("model policy plugin: policy version failed", Some(&e)))
    }

    /// The immutable snapshot of `policy_version` (settlement reads the
    /// version a turn was planned under, never the current one).
    ///
    /// # Errors
    /// `Internal` when the plugin cannot be resolved, fails or does not know
    /// the version.
    pub async fn snapshot_at(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.resolve().await?;
        plugin
            .get_policy_snapshot(user_id, policy_version)
            .await
            .map_err(|e| internal("model policy plugin: policy snapshot failed", Some(&e)))
    }

    /// Publishes one usage event to the plugin. Never an error: the outcome is
    /// what the outbox handler needs (`Retry` also covers a plugin that cannot
    /// be resolved).
    pub async fn publish_usage(&self, event: UsageEvent) -> UsageDeliveryOutcome {
        let Ok(plugin) = self.resolve().await else {
            return UsageDeliveryOutcome::Retry;
        };
        let result = tokio::time::timeout(USAGE_PLUGIN_TIMEOUT, plugin.publish_usage(event)).await;
        delivery_outcome(result)
    }

    /// Per-user limits under a policy version.
    ///
    /// # Errors
    /// `Internal` when the plugin cannot be resolved or fails.
    pub async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        let plugin = self.resolve().await?;
        plugin
            .get_user_limits(user_id, policy_version)
            .await
            .map_err(|e| internal("model policy plugin: user limits failed", Some(&e)))
    }
}

#[async_trait]
impl ModelPolicyPort for ModelPolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        Self::current_snapshot(self, user_id).await
    }

    async fn current_policy_version(&self, user_id: Uuid) -> Result<u64, DomainError> {
        Self::current_policy_version(self, user_id).await
    }

    async fn snapshot_at(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        Self::snapshot_at(self, user_id, policy_version).await
    }

    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        Self::user_limits(self, user_id, policy_version).await
    }
}
