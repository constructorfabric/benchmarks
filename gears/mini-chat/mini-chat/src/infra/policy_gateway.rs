//! Model policy gateway (DESIGN section 3.2 "Model policy gateway", section
//! 5.2, ADR-0008).
//!
//! [`ModelPolicyGateway`] resolves the model-policy plugin instance of the
//! gear's configured `vendor` through types-registry and the scoped
//! `ClientHub`, and itself implements [`MiniChatModelPolicyPluginClientV1`] by
//! delegation, so the domain only depends on
//! `Arc<dyn MiniChatModelPolicyPluginClientV1>`.
//!
//! Resolution is lazy: nothing is looked up before the first call. The
//! selected instance id is cached; it is reset when the instance's client is
//! missing from `ClientHub`, and a failed lookup is not cached. Snapshots and
//! limits are never cached: every call is forwarded to the plugin (section
//! 5.2.3 "Snapshot Delivery", section 3.3 Models API).
//!
//! The plugin lookup itself is the [`PluginLocator`] port, shared with the
//! audit gateway ([`crate::infra::audit_gateway`]); [`HubPluginLocator`] is
//! the types-registry + `ClientHub` implementation, using the platform's
//! vendor/priority selection (`choose_plugin_instance`).

use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, PublishError, UsageEvent, UserLicenseStatus,
    UserLimits,
};
use thiserror::Error;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

/// Failure to look a plugin instance up in types-registry.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginLookupError {
    /// No instance of the plugin type is registered for the vendor.
    #[error("no {plugin} plugin registered for vendor '{vendor}'")]
    NotRegistered {
        /// Plugin kind, e.g. `model policy`.
        plugin: &'static str,
        /// Requested vendor.
        vendor: String,
    },
    /// types-registry is unavailable (client missing or the query failed).
    #[error("{0}")]
    Unavailable(String),
    /// A registered instance of the plugin type is malformed.
    #[error("{0}")]
    InvalidInstance(String),
}

/// Plugin lookup port: finds the selected plugin instance and its scoped
/// client.
#[async_trait]
pub trait PluginLocator<C: ?Sized>: Send + Sync {
    /// Looks up the id of the plugin instance selected for the vendor.
    ///
    /// # Errors
    ///
    /// [`PluginLookupError::NotRegistered`] when no instance matches the
    /// vendor, [`PluginLookupError::Unavailable`] when types-registry cannot
    /// be queried, [`PluginLookupError::InvalidInstance`] when a registered
    /// instance is malformed.
    async fn find_instance(&self) -> Result<String, PluginLookupError>;

    /// The client registered under `instance_id`, if any.
    fn client(&self, instance_id: &str) -> Option<Arc<C>>;
}

/// [`PluginLocator`] over types-registry and the scoped `ClientHub`.
///
/// Lists the instances of the plugin spec `S`, selects the one of `vendor`
/// with the lowest priority, and returns the `C` client registered under the
/// instance id.
pub struct HubPluginLocator<S, C: ?Sized> {
    hub: Arc<ClientHub>,
    vendor: String,
    plugin: &'static str,
    _spec: PhantomData<fn() -> S>,
    _client: PhantomData<fn() -> Arc<C>>,
}

impl<S, C: ?Sized> HubPluginLocator<S, C> {
    /// Creates a locator for `plugin` (a human-readable kind used in error
    /// messages) instances of `vendor`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: impl Into<String>, plugin: &'static str) -> Self {
        Self {
            hub,
            vendor: vendor.into(),
            plugin,
            _spec: PhantomData,
            _client: PhantomData,
        }
    }
}

#[async_trait]
impl<S, C> PluginLocator<C> for HubPluginLocator<S, C>
where
    S: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema + 'static,
    C: ?Sized + Send + Sync + 'static,
{
    async fn find_instance(&self) -> Result<String, PluginLookupError> {
        let plugin = self.plugin;
        let registry = self.hub.get::<dyn TypesRegistryClient>().map_err(|err| {
            PluginLookupError::Unavailable(format!(
                "{plugin} plugin lookup failed: types-registry client unavailable: {err}"
            ))
        })?;
        let type_id = <S as gts::GtsSchema>::TYPE_ID;
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|err| {
                PluginLookupError::Unavailable(format!(
                    "{plugin} plugin lookup failed: types-registry query failed: {err}"
                ))
            })?;
        choose_plugin_instance::<S>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|err| match err {
            ChoosePluginError::PluginNotFound { vendor, .. } => {
                PluginLookupError::NotRegistered { plugin, vendor }
            }
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                PluginLookupError::InvalidInstance(format!(
                    "invalid {plugin} plugin instance '{gts_id}': {reason}"
                ))
            }
        })
    }

    fn client(&self, instance_id: &str) -> Option<Arc<C>> {
        self.hub
            .try_get_scoped::<C>(&ClientScope::gts_id(instance_id))
    }
}

/// Locator type of the model policy plugin.
pub type ModelPolicyPluginLocator = dyn PluginLocator<dyn MiniChatModelPolicyPluginClientV1>;

/// Gateway to the model policy plugin selected for the gear's vendor.
pub struct ModelPolicyGateway {
    locator: Arc<ModelPolicyPluginLocator>,
    selector: GtsPluginSelector,
}

impl ModelPolicyGateway {
    /// Gateway resolving the plugin of `vendor` through types-registry and
    /// the scoped `ClientHub`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        Self::with_locator(Arc::new(HubPluginLocator::<
            MiniChatModelPolicyPluginSpecV1,
            dyn MiniChatModelPolicyPluginClientV1,
        >::new(hub, vendor, "model policy")))
    }

    /// Gateway over an arbitrary [`PluginLocator`].
    #[must_use]
    pub fn with_locator(locator: Arc<ModelPolicyPluginLocator>) -> Self {
        Self {
            locator,
            selector: GtsPluginSelector::new(),
        }
    }

    /// Resolves the plugin client: the cached instance id (looked up on
    /// first use), then its scoped client. A missing client resets the cache.
    async fn plugin(
        &self,
    ) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, MiniChatModelPolicyPluginError> {
        let instance_id = self
            .selector
            .get_or_init(|| self.locator.find_instance())
            .await
            .map_err(|err| match err {
                PluginLookupError::InvalidInstance(msg) => {
                    MiniChatModelPolicyPluginError::Internal(msg)
                }
                other @ (PluginLookupError::NotRegistered { .. }
                | PluginLookupError::Unavailable(_)) => {
                    MiniChatModelPolicyPluginError::Transient(other.to_string())
                }
            })?;
        if let Some(client) = self.locator.client(&instance_id) {
            return Ok(client);
        }
        self.selector.reset().await;
        tracing::warn!(
            target: "mini_chat.policy_gateway",
            instance_id = %instance_id,
            "model policy plugin client not registered; cached instance id reset"
        );
        Err(MiniChatModelPolicyPluginError::Transient(format!(
            "model policy plugin client not registered for instance '{instance_id}'"
        )))
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for ModelPolicyGateway {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        self.plugin()
            .await?
            .get_current_policy_version(user_id)
            .await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        self.plugin()
            .await?
            .get_policy_snapshot(user_id, policy_version)
            .await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        self.plugin()
            .await?
            .get_user_limits(user_id, policy_version)
            .await
    }

    /// A resolution failure is transient: the usage outbox handler retries
    /// (DESIGN section 5.6).
    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        let plugin = self
            .plugin()
            .await
            .map_err(|err| PublishError::Transient(err.to_string()))?;
        plugin.publish_usage(payload).await
    }

    async fn check_user_license(
        &self,
        user_id: Uuid,
    ) -> Result<UserLicenseStatus, MiniChatModelPolicyPluginError> {
        self.plugin().await?.check_user_license(user_id).await
    }
}

#[cfg(test)]
#[path = "policy_gateway_tests.rs"]
mod tests;
