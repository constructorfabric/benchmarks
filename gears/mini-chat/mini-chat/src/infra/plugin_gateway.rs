//! Lazy plugin resolution through types-registry (vendor + lowest priority).

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginError, MiniChatModelPolicyPluginSpecV1, PolicySnapshot, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Outcome of resolving a plugin.
pub enum Resolved<T: ?Sized> {
    Found(Arc<T>),
    /// No instance registered for the vendor.
    NotRegistered,
    /// Resolution failed or the client is not in `ClientHub` yet.
    Unavailable(String),
}

struct HubSource {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

async fn resolve_instance<S>(hub: &ClientHub, vendor: &str) -> Result<String, ResolveErr>
where
    S: gts::GtsSchema + for<'de> gts::GtsDeserialize<'de>,
{
    let registry = hub
        .get::<dyn TypesRegistryClient>()
        .map_err(|e| ResolveErr::Unavailable(format!("types-registry unavailable: {e}")))?;
    let type_id = S::TYPE_ID;
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
        .await
        .map_err(|e| ResolveErr::Unavailable(format!("types-registry list failed: {e}")))?;
    choose_plugin_instance::<S>(vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => ResolveErr::NotFound,
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                ResolveErr::Unavailable(format!("invalid plugin instance {gts_id}: {reason}"))
            }
        })
}

enum ResolveErr {
    NotFound,
    Unavailable(String),
}

impl HubSource {
    async fn get<S, T>(&self) -> Resolved<T>
    where
        S: gts::GtsSchema + for<'de> gts::GtsDeserialize<'de>,
        T: ?Sized + Send + Sync + 'static,
    {
        let id = match self
            .selector
            .get_or_init(|| resolve_instance::<S>(&self.hub, &self.vendor))
            .await
        {
            Ok(id) => id,
            Err(ResolveErr::NotFound) => return Resolved::NotRegistered,
            Err(ResolveErr::Unavailable(e)) => return Resolved::Unavailable(e),
        };
        if let Some(c) = self
            .hub
            .try_get_scoped::<T>(&ClientScope::gts_id(id.as_ref()))
        {
            Resolved::Found(c)
        } else {
            self.selector.reset().await;
            Resolved::Unavailable(format!("plugin {id} client not registered in ClientHub"))
        }
    }
}

enum Source<T: ?Sized> {
    Hub(HubSource),
    Fixed(Option<Arc<T>>),
}

/// Model policy gateway.
pub struct PolicyGateway {
    src: Source<dyn MiniChatModelPolicyPluginClientV1>,
}

impl PolicyGateway {
    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            src: Source::Hub(HubSource {
                hub,
                vendor,
                selector: GtsPluginSelector::new(),
            }),
        }
    }

    #[must_use]
    pub fn fixed(client: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self {
            src: Source::Fixed(Some(client)),
        }
    }

    /// Resolves the plugin client.
    pub async fn resolve(&self) -> Resolved<dyn MiniChatModelPolicyPluginClientV1> {
        match &self.src {
            Source::Fixed(Some(c)) => Resolved::Found(Arc::clone(c)),
            Source::Fixed(None) => Resolved::NotRegistered,
            Source::Hub(h) => {
                h.get::<MiniChatModelPolicyPluginSpecV1, dyn MiniChatModelPolicyPluginClientV1>()
                    .await
            }
        }
    }

    async fn client(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        match self.resolve().await {
            Resolved::Found(c) => Ok(c),
            Resolved::NotRegistered => Err(DomainError::internal(
                "model policy plugin is not registered",
            )),
            Resolved::Unavailable(e) => Err(DomainError::internal(format!(
                "model policy plugin unavailable: {e}"
            ))),
        }
    }

    /// Current policy snapshot (version lookup + snapshot fetch, no cache — ADR-0008).
    ///
    /// # Errors
    /// Internal error when the plugin cannot be resolved or fails.
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let c = self.client().await?;
        let v = c
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| map_plugin_err(&e))?;
        c.get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| map_plugin_err(&e))
    }

    /// Snapshot of a specific version (settlement).
    ///
    /// # Errors
    /// Internal error when the plugin cannot be resolved or fails.
    pub async fn snapshot(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        let c = self.client().await?;
        c.get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| map_plugin_err(&e))
    }

    /// User limits for a version.
    ///
    /// # Errors
    /// Internal error when the plugin cannot be resolved or fails.
    pub async fn user_limits(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<UserLimits, DomainError> {
        let c = self.client().await?;
        c.get_user_limits(user_id, version)
            .await
            .map_err(|e| map_plugin_err(&e))
    }
}

fn map_plugin_err(e: &MiniChatModelPolicyPluginError) -> DomainError {
    DomainError::internal(format!("model policy plugin error: {e}"))
}

/// Audit gateway. "Not registered" is never cached.
pub struct AuditGateway {
    src: Source<dyn MiniChatAuditPluginClientV1>,
}

impl AuditGateway {
    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            src: Source::Hub(HubSource {
                hub,
                vendor,
                selector: GtsPluginSelector::new(),
            }),
        }
    }

    #[must_use]
    pub fn fixed(client: Option<Arc<dyn MiniChatAuditPluginClientV1>>) -> Self {
        Self {
            src: Source::Fixed(client),
        }
    }

    pub async fn resolve(&self) -> Resolved<dyn MiniChatAuditPluginClientV1> {
        match &self.src {
            Source::Fixed(Some(c)) => Resolved::Found(Arc::clone(c)),
            Source::Fixed(None) => Resolved::NotRegistered,
            Source::Hub(h) => {
                h.get::<MiniChatAuditPluginSpecV1, dyn MiniChatAuditPluginClientV1>()
                    .await
            }
        }
    }
}
