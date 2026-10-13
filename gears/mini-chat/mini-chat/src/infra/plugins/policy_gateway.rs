//! Model policy gateway: resolves the policy plugin lazily through
//! types-registry and adapts it to the [`PolicyProvider`] port.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::GtsPluginSelector;
use toolkit::telemetry::ThrottledLog;
use uuid::Uuid;

use super::resolve_plugin_instance;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyProvider;

/// Throttle interval for "plugin client not registered yet" warnings.
const UNAVAILABLE_LOG_THROTTLE: Duration = Duration::from_secs(10);

pub struct PolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    unavailable_log: ThrottledLog,
}

impl PolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            unavailable_log: ThrottledLog::new(UNAVAILABLE_LOG_THROTTLE),
        }
    }

    /// Resolves the plugin lazily; the chosen instance id is cached.
    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, String> {
        let instance_id = self
            .selector
            .get_or_init(|| async {
                resolve_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(&self.hub, &self.vendor)
                    .await
                    .map_err(|e| match e {
                        super::ResolveError::NoPlugin(m) | super::ResolveError::Failed(m) => m,
                    })
            })
            .await?;
        let scope = ClientScope::gts_id(instance_id.as_ref());
        if let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&scope)
        {
            return Ok(client);
        }
        if self.unavailable_log.should_log() {
            tracing::warn!(
                instance_id = %instance_id,
                "mini-chat model policy plugin client is not registered in ClientHub yet"
            );
        }
        Err(format!(
            "model policy plugin client not registered for '{instance_id}'"
        ))
    }

    async fn domain_plugin(
        &self,
    ) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        self.plugin()
            .await
            .map_err(|m| DomainError::Internal(format!("model policy plugin: {m}")))
    }
}

fn plugin_err(e: &MiniChatModelPolicyPluginError) -> DomainError {
    DomainError::Internal(format!("model policy plugin: {e}"))
}

#[async_trait]
impl PolicyProvider for PolicyGateway {
    async fn current(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        let plugin = self.domain_plugin().await?;
        let info = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| plugin_err(&e))?;
        let snapshot = plugin
            .get_policy_snapshot(user_id, info.policy_version)
            .await
            .map_err(|e| plugin_err(&e))?;
        Ok(Arc::new(snapshot))
    }

    async fn snapshot(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        let snapshot = self
            .domain_plugin()
            .await?
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| match e {
                // The version was dropped: permanent, distinguishable from a
                // transient plugin failure.
                MiniChatModelPolicyPluginError::NotFound(m) => DomainError::PolicySnapshotGone(
                    format!("model policy plugin: policy version {version}: {m}"),
                ),
                other => plugin_err(&other),
            })?;
        Ok(Arc::new(snapshot))
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        self.domain_plugin()
            .await?
            .get_user_limits(user_id, version)
            .await
            .map_err(|e| plugin_err(&e))
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError> {
        let plugin = self
            .plugin()
            .await
            .map_err(|m| PublishError::Transient(format!("model policy plugin: {m}")))?;
        plugin.publish_usage(ev).await
    }
}
