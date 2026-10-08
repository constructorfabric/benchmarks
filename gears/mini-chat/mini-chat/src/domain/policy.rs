//! Model policy gateway: resolves the `mini-chat-model-policy-plugin`
//! instance via types-registry (lazily) and exposes snapshot / limits /
//! usage publication.

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry,
    PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use super::error::DomainError;

/// Plugin resolution failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginResolveError {
    #[error("no plugin instance registered")]
    NotRegistered,
    #[error("plugin client not available in ClientHub")]
    ClientMissing,
    #[error("plugin resolution failed: {0}")]
    Failed(String),
}

/// Lazily-resolved scoped plugin client (shared by the policy and audit gateways).
pub struct PluginResolver {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl PluginResolver {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self { hub, vendor, selector: GtsPluginSelector::new() }
    }

    async fn resolve_instance<P>(&self) -> Result<String, PluginResolveError>
    where
        P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
    {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| PluginResolveError::Failed(e.to_string()))?;
        let type_id = <P as gts::GtsSchema>::TYPE_ID;
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| PluginResolveError::Failed(e.to_string()))?;
        choose_plugin_instance::<P>(&self.vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))
            .map_err(|e| match e {
                ChoosePluginError::PluginNotFound { .. } => PluginResolveError::NotRegistered,
                other @ ChoosePluginError::InvalidPluginInstance { .. } => PluginResolveError::Failed(other.to_string()),
            })
    }

    /// Resolve the plugin client `T` registered under the instance of spec `P`.
    ///
    /// # Errors
    /// See [`PluginResolveError`]. Errors are not cached.
    pub async fn get<P, T>(&self) -> Result<Arc<T>, PluginResolveError>
    where
        P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
        T: ?Sized + Send + Sync + 'static,
    {
        let id = self.selector.get_or_init(|| self.resolve_instance::<P>()).await?;
        if let Some(c) = self.hub.try_get_scoped::<T>(&ClientScope::gts_id(id.as_ref())) {
            Ok(c)
        } else {
            self.selector.reset().await;
            Err(PluginResolveError::ClientMissing)
        }
    }
}

/// Model policy gateway.
pub struct PolicyGateway {
    resolver: PluginResolver,
}

/// A resolved policy view for one request.
#[derive(Debug, Clone)]
pub struct PolicyView {
    pub version: u64,
    pub snapshot: Arc<PolicySnapshot>,
}

impl PolicyView {
    /// Catalog entry by id (enabled or not).
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.snapshot.find_model(id)
    }

    /// Enabled catalog entry by id.
    #[must_use]
    pub fn find_enabled(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.snapshot.find_model(id).filter(|m| m.enabled)
    }

    /// Default model: first enabled `is_default`, else first enabled.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        let mut enabled = self.snapshot.enabled_models();
        self.snapshot
            .enabled_models()
            .find(|m| m.is_default())
            .or_else(|| enabled.next())
    }
}

impl PolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self { resolver: PluginResolver::new(hub, vendor) }
    }

    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        self.resolver
            .get::<MiniChatModelPolicyPluginSpecV1, dyn MiniChatModelPolicyPluginClientV1>()
            .await
            .map_err(|e| DomainError::Internal(format!("model policy plugin: {e}")))
    }

    /// Current policy version and its snapshot.
    ///
    /// # Errors
    /// `Internal` on any plugin failure.
    pub async fn current(&self, user_id: Uuid) -> Result<PolicyView, DomainError> {
        let plugin = self.plugin().await?;
        let v = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::Internal(format!("policy version: {e}")))?;
        let snapshot = plugin
            .get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::Internal(format!("policy snapshot: {e}")))?;
        Ok(PolicyView { version: v.policy_version, snapshot: Arc::new(snapshot) })
    }

    /// Snapshot of a specific version (settlement).
    ///
    /// # Errors
    /// `Internal` on any plugin failure.
    pub async fn snapshot(&self, user_id: Uuid, version: u64) -> Result<PolicyView, DomainError> {
        let plugin = self.plugin().await?;
        let snapshot = plugin
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::Internal(format!("policy snapshot: {e}")))?;
        Ok(PolicyView { version, snapshot: Arc::new(snapshot) })
    }

    /// Per-user limits bound to a policy version.
    ///
    /// # Errors
    /// `Internal` on any plugin failure.
    pub async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let plugin = self.plugin().await?;
        plugin
            .get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::Internal(format!("user limits: {e}")))
    }

    /// Publish a usage event through the plugin.
    ///
    /// # Errors
    /// `Err(true)` for retryable failures, `Err(false)` for permanent ones.
    #[allow(clippy::cognitive_complexity, reason = "plugin resolution and outcome classification")]
    pub async fn publish_usage(&self, ev: UsageEvent) -> Result<(), bool> {
        let plugin = match self
            .resolver
            .get::<MiniChatModelPolicyPluginSpecV1, dyn MiniChatModelPolicyPluginClientV1>()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "usage publish: model policy plugin unavailable");
                return Err(true);
            }
        };
        match plugin.publish_usage(ev).await {
            Ok(()) => Ok(()),
            Err(PublishError::Transient(e)) => {
                tracing::warn!(error = %e, "usage publish transient failure");
                Err(true)
            }
            Err(PublishError::Permanent(e)) => {
                tracing::error!(error = %e, "usage publish permanent failure");
                Err(false)
            }
        }
    }
}
