//! Model policy gateway: resolves the model-policy plugin through the types registry (lazily, at
//! first use) and serves the policy to the domain.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelPolicyPluginError,
    PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::GtsPluginSelector;
use tracing::warn;
use uuid::Uuid;

use super::plugin_select::{PluginResolutionError, select_instance};
use crate::domain::error::DomainError;
use crate::domain::ports::{ModelPolicyGateway, UsagePublication};

/// Gateway to the selected model-policy plugin. The plugin is resolved on first use, because the
/// plugin gears initialize after this one; the selected instance id is cached.
pub struct GtsModelPolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl GtsModelPolicyGateway {
    /// Creates a gateway selecting plugin instances of `vendor`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        let instance_id = self
            .selector
            .get_or_init(|| async {
                select_instance::<MiniChatModelPolicyPluginSpecV1>(&self.hub, &self.vendor).await
            })
            .await
            .map_err(|err| match err {
                PluginResolutionError::NotFound { vendor } => DomainError::PolicyUnavailable {
                    diagnostic: format!("no model policy plugin registered for vendor `{vendor}`"),
                },
                PluginResolutionError::RegistryUnavailable(cause)
                | PluginResolutionError::InvalidInstance(cause) => DomainError::PolicyUnavailable {
                    diagnostic: format!("model policy plugin resolution failed: {cause}"),
                },
            })?;
        self.hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(
                instance_id.as_ref(),
            ))
            .ok_or_else(|| {
                warn!(instance = %instance_id, "model policy plugin client is not registered");
                DomainError::PolicyUnavailable {
                    diagnostic: format!(
                        "model policy plugin client `{instance_id}` is not registered"
                    ),
                }
            })
    }
}

/// The domain error of a failed snapshot lookup by version: only an outage of the plugin is
/// transient (`PolicyUnavailable`, retried); a version the plugin does not know and a failure a
/// retry will not fix are `Internal`, which the orphan watchdog finalizes with a released reserve.
fn map_snapshot_error(err: &ModelPolicyPluginError) -> DomainError {
    match err {
        ModelPolicyPluginError::Unavailable(_) => DomainError::PolicyUnavailable {
            diagnostic: format!("policy snapshot lookup failed: {err}"),
        },
        ModelPolicyPluginError::PolicyVersionNotFound { .. }
        | ModelPolicyPluginError::Internal(_) => {
            DomainError::internal(format!("policy snapshot lookup failed: {err}"))
        }
    }
}

#[async_trait]
impl ModelPolicyGateway for GtsModelPolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await?;
        let version = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|err| DomainError::PolicyUnavailable {
                diagnostic: format!("policy version lookup failed: {err}"),
            })?;
        plugin
            .get_policy_snapshot(user_id, version.policy_version)
            .await
            .map_err(|err| DomainError::PolicyUnavailable {
                diagnostic: format!("policy snapshot lookup failed: {err}"),
            })
    }

    async fn snapshot_at(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await?;
        plugin
            .get_policy_snapshot(user_id, policy_version)
            .await
            .map_err(|err| map_snapshot_error(&err))
    }

    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, DomainError> {
        let plugin = self.plugin().await?;
        plugin
            .get_user_limits(user_id, policy_version)
            .await
            .map_err(|err| DomainError::PolicyUnavailable {
                diagnostic: format!("user limits lookup failed: {err}"),
            })
    }

    async fn publish_usage(&self, event: UsageEvent) -> UsagePublication {
        let plugin = match self.plugin().await {
            Ok(plugin) => plugin,
            Err(err) => return UsagePublication::Retry(err.to_string()),
        };
        match plugin.publish_usage(event).await {
            Ok(()) => UsagePublication::Published,
            Err(PublishError::Transient(cause)) => UsagePublication::Retry(cause),
            Err(PublishError::Permanent(cause)) => UsagePublication::Reject(cause),
        }
    }
}

#[cfg(test)]
#[path = "model_policy_gateway_tests.rs"]
mod tests;
