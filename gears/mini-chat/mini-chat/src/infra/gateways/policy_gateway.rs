//! Policy gateway: resolves the model policy plugin through types-registry
//! (`choose_plugin_instance` by `vendor`) and adapts it to [`PolicyPort`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicyPluginError,
    PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use toolkit::telemetry::ThrottledLog;
use tracing::{info, warn};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::PolicyPort;

const UNAVAILABLE_LOG_THROTTLE: Duration = Duration::from_secs(10);

/// [`PolicyPort`] backed by the model policy plugin selected via types-registry.
///
/// Resolution is lazy (first call) and the selected instance id is cached;
/// the cache is reset when the instance has no client in `ClientHub`.
pub struct PolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    unavailable_log: ThrottledLog,
}

impl PolicyGateway {
    /// Create a gateway selecting plugin instances of `vendor`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        Self {
            hub,
            vendor: vendor.into(),
            selector: GtsPluginSelector::new(),
            unavailable_log: ThrottledLog::new(UNAVAILABLE_LOG_THROTTLE),
        }
    }

    async fn resolve_instance(&self) -> Result<String, DomainError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::PluginUnavailable(format!("types-registry: {e}")))?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id().clone();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| DomainError::PluginUnavailable(format!("types-registry: {e}")))?;
        let id = choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| DomainError::PluginUnavailable(e.to_string()))?;
        info!(plugin_gts_id = %id, vendor = %self.vendor, "selected mini-chat model policy plugin");
        Ok(id)
    }

    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        let id = self
            .selector
            .get_or_init(|| self.resolve_instance())
            .await
            .inspect_err(|e| {
                if self.unavailable_log.should_log() {
                    warn!(vendor = %self.vendor, error = %e, "model policy plugin not resolvable");
                }
            })?;
        if let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(&id))
        {
            return Ok(client);
        }
        self.selector.reset().await;
        if self.unavailable_log.should_log() {
            warn!(plugin_gts_id = %id, vendor = %self.vendor, "model policy plugin client not registered");
        }
        Err(DomainError::PluginUnavailable(format!(
            "model policy plugin {id}: client not registered"
        )))
    }
}

fn map_plugin_error(e: PolicyPluginError) -> DomainError {
    match e {
        PolicyPluginError::Unavailable(msg) => DomainError::PluginUnavailable(msg),
        other @ (PolicyPluginError::VersionNotFound { .. } | PolicyPluginError::Internal(_)) => {
            DomainError::Internal(other.to_string())
        }
    }
}

#[async_trait]
impl PolicyPort for PolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        let plugin = self.plugin().await?;
        let version = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(map_plugin_error)?;
        let snapshot = plugin
            .get_policy_snapshot(user_id, version.policy_version)
            .await
            .map_err(map_plugin_error)?;
        Ok(Arc::new(snapshot))
    }

    async fn snapshot_for_version(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        let plugin = self.plugin().await?;
        let snapshot = plugin
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(map_plugin_error)?;
        Ok(Arc::new(snapshot))
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let plugin = self.plugin().await?;
        plugin
            .get_user_limits(user_id, version)
            .await
            .map_err(map_plugin_error)
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError> {
        let plugin = self
            .plugin()
            .await
            .map_err(|e| PublishError::Transient(e.to_string()))?;
        plugin.publish_usage(ev).await
    }
}

#[cfg(test)]
#[path = "policy_gateway_tests.rs"]
mod tests;
