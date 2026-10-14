//! The model-policy gateway over the plugin resolved through types-registry.
//!
//! The plugin instance is resolved lazily for the spec `MiniChatModelPolicyPluginSpecV1`
//! (instances with the configured `vendor`, lowest `priority` wins) and its scoped
//! `MiniChatModelPolicyPluginClientV1` is taken from the `ClientHub`. There is no cache:
//! every call asks the plugin. `check_user_license` is never called.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelPolicyPluginError,
    PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::quota::{UsagePublishError, UsagePublisher};

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-no-cache
/// Types-registry-backed [`ModelPolicyGateway`]. It holds the resolved plugin instance id
/// and nothing else: no snapshot cache, no snapshot table and no notify endpoint (ADR-0008).
pub struct GtsModelPolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-no-cache

impl GtsModelPolicyGateway {
    /// Create the gateway; the plugin is resolved on first use.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    /// Select the plugin instance among those of the configured vendor, lowest `priority`
    /// first, and cache it (a failure is never cached).
    async fn resolve_instance(&self) -> Result<String, DomainError> {
        let registry = self.hub.get::<dyn TypesRegistryClient>().map_err(|e| {
            DomainError::PolicyUnavailable {
                cause: format!("types-registry client unavailable: {e}"),
            }
        })?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
        let type_prefix: &str = type_id.as_ref();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| DomainError::PolicyUnavailable {
                cause: format!("types-registry list_instances failed: {e}"),
            })?;
        choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances
                .iter()
                // only instances of this plugin spec compete
                .filter(|instance| instance.id.as_ref().starts_with(type_prefix))
                .map(|instance| (instance.id.as_ref(), &instance.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { vendor, .. } => DomainError::PolicyUnavailable {
                cause: format!("no model-policy plugin registered for vendor `{vendor}`"),
            },
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                DomainError::PolicyUnavailable {
                    cause: format!("invalid model-policy plugin instance `{gts_id}`: {reason}"),
                }
            }
        })
    }

    /// The scoped plugin client of the resolved instance.
    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-resolve-plugin
        let instance_id = self
            .selector
            .get_or_init(|| self.resolve_instance())
            .await?;
        let scope = ClientScope::gts_id(instance_id.as_ref());
        let client = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&scope);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-resolve-plugin
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-failed
        if let Some(client) = client {
            Ok(client)
        } else {
            // the client may register later: select again on the next call
            self.selector.reset().await;
            Err(DomainError::PolicyUnavailable {
                cause: format!("model-policy plugin client `{instance_id}` is not registered"),
            })
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-failed
    }
}

/// A plugin call failure; the cause stays in the diagnostic and is never returned to a client.
fn plugin_failure(what: &str, error: &ModelPolicyPluginError) -> DomainError {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-return-error
    DomainError::PolicyUnavailable {
        cause: format!("model-policy plugin call `{what}` failed: {error}"),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-return-error
}

#[async_trait]
impl ModelPolicyGateway for GtsModelPolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-call-snapshot
        let version = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| plugin_failure("get_current_policy_version", &e))?;
        let snapshot = plugin
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| plugin_failure("get_policy_snapshot", &e))?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-call-snapshot
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-return
        Ok(snapshot)
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-return
    }

    async fn snapshot_of_version(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        let plugin = self.plugin().await?;
        plugin
            .get_policy_snapshot(user_id, policy_version)
            .await
            .map_err(|e| plugin_failure("get_policy_snapshot", &e))
    }

    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        let plugin = self.plugin().await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-user-limits
        // the limits are a separate call and are never merged into the shared snapshot
        plugin
            .get_user_limits(user_id, policy_version)
            .await
            .map_err(|e| plugin_failure("get_user_limits", &e))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-policy-snapshot-access:p1:inst-policy-user-limits
    }
}

/// The usage publication through the resolved plugin (`publish_usage`).
#[async_trait]
impl UsagePublisher for GtsModelPolicyGateway {
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), UsagePublishError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-resolve-plugin
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-catch-transient
        // a plugin that cannot be resolved is a transient failure: the handler retries, so no
        // committed usage is lost
        let plugin = self
            .plugin()
            .await
            .map_err(|e| UsagePublishError::Transient(e.to_string()))?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-catch-transient
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-resolve-plugin
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-publish
        // the event goes to the plugin unchanged, outside any database transaction
        plugin.publish_usage(event).await.map_err(|e| match e {
            PublishError::Transient(cause) => UsagePublishError::Transient(cause),
            PublishError::Permanent(cause) => UsagePublishError::Permanent(cause),
        })
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-publish
    }
}

#[cfg(test)]
#[path = "policy_gateway_tests.rs"]
mod policy_gateway_tests;
