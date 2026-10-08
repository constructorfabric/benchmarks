//! Model policy gateway: resolves the `mini-chat-model-policy-plugin` lazily
//! through types-registry and serves the policy snapshot and user limits.
//! There is no snapshot cache (ADR-0008): every call asks the plugin.

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry,
    ModelTier, PolicySnapshot, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::{DomainError, DomainResult};

/// Resolves plugin instances of one GTS plugin type.
pub struct PluginResolver {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

/// Outcome of a plugin lookup.
pub enum PluginLookup<T: ?Sized> {
    Found(Arc<T>),
    /// No instance of the plugin type is registered for the vendor.
    NotRegistered,
    /// The instance resolves in types-registry but its client is missing from `ClientHub`.
    ClientMissing(String),
}

impl PluginResolver {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    /// Looks the plugin up; a missing instance is not cached.
    ///
    /// # Errors
    /// types-registry failures.
    pub async fn lookup<T, P>(&self) -> DomainResult<PluginLookup<T>>
    where
        T: ?Sized + Send + Sync + 'static,
        P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
    {
        let hub = Arc::clone(&self.hub);
        let vendor = self.vendor.clone();
        let res = self
            .selector
            .get_or_init(|| async move {
                let registry = hub
                    .get::<dyn TypesRegistryClient>()
                    .map_err(|e| DomainError::internal(format!("types-registry client: {e}")))?;
                let type_id = P::TYPE_ID;
                let instances = registry
                    .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
                    .await
                    .map_err(|e| DomainError::internal(format!("types-registry list: {e}")))?;
                choose_plugin_instance::<P>(
                    &vendor,
                    instances.iter().map(|e| (e.id.as_ref(), &e.object)),
                )
                .map_err(|e| match e {
                    toolkit::plugins::ChoosePluginError::PluginNotFound { .. } => {
                        DomainError::NotFound(crate::domain::error::Resource::Model)
                    }
                    other @ toolkit::plugins::ChoosePluginError::InvalidPluginInstance {
                        ..
                    } => DomainError::internal(format!("plugin selection: {other}")),
                })
            })
            .await;
        let id = match res {
            Ok(id) => id,
            Err(DomainError::NotFound(_)) => return Ok(PluginLookup::NotRegistered),
            Err(e) => return Err(e),
        };
        let scope = ClientScope::gts_id(id.as_ref());
        if let Some(client) = self.hub.try_get_scoped::<T>(&scope) {
            Ok(PluginLookup::Found(client))
        } else {
            self.selector.reset().await;
            Ok(PluginLookup::ClientMissing(id.to_string()))
        }
    }
}

/// Model policy gateway.
pub struct PolicyGateway {
    resolver: PluginResolver,
}

impl PolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            resolver: PluginResolver::new(hub, vendor),
        }
    }

    /// The model policy plugin client.
    ///
    /// # Errors
    /// `Internal` when the plugin is not available.
    pub async fn plugin(&self) -> DomainResult<Arc<dyn MiniChatModelPolicyPluginClientV1>> {
        match self
            .resolver
            .lookup::<dyn MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1>()
            .await?
        {
            PluginLookup::Found(p) => Ok(p),
            PluginLookup::NotRegistered => Err(DomainError::internal(
                "no model policy plugin is registered",
            )),
            PluginLookup::ClientMissing(id) => Err(DomainError::internal(format!(
                "model policy plugin {id} has no client registered"
            ))),
        }
    }

    /// Current policy snapshot for the user.
    ///
    /// # Errors
    /// Plugin failures (500).
    pub async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<PolicySnapshot> {
        let plugin = self.plugin().await?;
        let version = plugin
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        plugin
            .get_policy_snapshot(user_id, version.policy_version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Snapshot of a specific policy version (settlement).
    ///
    /// # Errors
    /// Plugin failures.
    pub async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<PolicySnapshot> {
        let plugin = self.plugin().await?;
        plugin
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot {version}: {e}")))
    }

    /// Credit limits of the user.
    ///
    /// # Errors
    /// Plugin failures.
    pub async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits> {
        let plugin = self.plugin().await?;
        plugin
            .get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("user limits: {e}")))
    }
}

/// Enabled catalog entries in catalog order.
pub fn enabled_models(snapshot: &PolicySnapshot) -> impl Iterator<Item = &ModelCatalogEntry> {
    snapshot.model_catalog.iter().filter(|m| m.enabled)
}

/// Default model of new chats: first enabled `is_default` entry, else the first enabled entry.
#[must_use]
pub fn default_model(snapshot: &PolicySnapshot) -> Option<&ModelCatalogEntry> {
    enabled_models(snapshot)
        .find(|m| m.is_default())
        .or_else(|| enabled_models(snapshot).next())
}

/// Candidate model of a tier for the cascade (DESIGN §4 "Downgrade Decision Flow").
#[must_use]
pub fn tier_candidate<'a>(
    snapshot: &'a PolicySnapshot,
    tier: ModelTier,
    selected: &str,
) -> Option<&'a ModelCatalogEntry> {
    let in_tier = || enabled_models(snapshot).filter(move |m| m.tier == tier);
    in_tier()
        .find(|m| m.id == selected)
        .or_else(|| in_tier().find(|m| m.is_default()))
        .or_else(|| in_tier().next())
}
