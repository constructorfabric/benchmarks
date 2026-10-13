//! Lazy plugin resolution for the model policy and audit plugins.
//!
//! Plugins are resolved through types-registry by vendor (lowest priority
//! wins) and fetched from ClientHub as scoped clients. Tests can use a fixed
//! client instead.

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

enum Source<T: ?Sized> {
    Hub {
        hub: Arc<ClientHub>,
        vendor: String,
        selector: GtsPluginSelector,
    },
    Fixed(Arc<T>),
}

/// Outcome of a plugin lookup.
pub enum Lookup<T: ?Sized> {
    Found(Arc<T>),
    /// No instance registered for the vendor.
    NotRegistered,
}

async fn resolve_instance<S>(hub: &ClientHub, vendor: &str) -> Result<Option<String>, String>
where
    S: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
{
    let registry = hub.get::<dyn TypesRegistryClient>().map_err(|e| e.to_string())?;
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{}*", <S as gts::GtsSchema>::TYPE_ID)))
        .await
        .map_err(|e| e.to_string())?;
    match choose_plugin_instance::<S>(vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object))) {
        Ok(id) => Ok(Some(id)),
        Err(ChoosePluginError::PluginNotFound { .. }) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Model policy plugin gateway.
pub struct PolicyGateway {
    source: Source<dyn MiniChatModelPolicyPluginClientV1>,
}

impl PolicyGateway {
    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            source: Source::Hub {
                hub,
                vendor,
                selector: GtsPluginSelector::new(),
            },
        }
    }

    #[must_use]
    pub fn fixed(client: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self {
            source: Source::Fixed(client),
        }
    }

    /// Resolves the plugin client.
    ///
    /// # Errors
    /// Internal error when no plugin can be resolved.
    pub async fn client(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        match &self.source {
            Source::Fixed(c) => Ok(Arc::clone(c)),
            Source::Hub { hub, vendor, selector } => {
                let id = selector
                    .get_or_init(|| async {
                        match resolve_instance::<MiniChatModelPolicyPluginSpecV1>(hub, vendor).await {
                            Ok(Some(id)) => Ok(id),
                            Ok(None) => Err(DomainError::internal("no model policy plugin registered")),
                            Err(e) => Err(DomainError::internal(format!("model policy plugin resolution: {e}"))),
                        }
                    })
                    .await?;
                if let Some(c) = hub.try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(id.as_ref())) {
                    Ok(c)
                } else {
                    selector.reset().await;
                    Err(DomainError::internal("model policy plugin client not registered"))
                }
            }
        }
    }

    /// Current policy snapshot for a user.
    ///
    /// # Errors
    /// Internal error on plugin failure.
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let c = self.client().await?;
        let v = c
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        c.get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Snapshot of a specific version (settlement).
    ///
    /// # Errors
    /// Internal error on plugin failure.
    pub async fn snapshot(&self, user_id: Uuid, version: u64) -> Result<PolicySnapshot, DomainError> {
        self.client()
            .await?
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Per-user limits for a version.
    ///
    /// # Errors
    /// Internal error on plugin failure.
    pub async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        self.client()
            .await?
            .get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("user limits: {e}")))
    }
}

/// Audit plugin gateway. "No plugin registered" is not cached.
pub struct AuditGateway {
    source: Source<dyn MiniChatAuditPluginClientV1>,
}

impl AuditGateway {
    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            source: Source::Hub {
                hub,
                vendor,
                selector: GtsPluginSelector::new(),
            },
        }
    }

    #[must_use]
    pub fn fixed(client: Arc<dyn MiniChatAuditPluginClientV1>) -> Self {
        Self {
            source: Source::Fixed(client),
        }
    }

    /// Looks up the audit plugin.
    ///
    /// # Errors
    /// A message on resolution failure or when the instance has no client.
    pub async fn lookup(&self) -> Result<Lookup<dyn MiniChatAuditPluginClientV1>, String> {
        match &self.source {
            Source::Fixed(c) => Ok(Lookup::Found(Arc::clone(c))),
            Source::Hub { hub, vendor, selector } => {
                let res = selector
                    .get_or_init(|| async {
                        match resolve_instance::<MiniChatAuditPluginSpecV1>(hub, vendor).await {
                            Ok(Some(id)) => Ok(id),
                            Ok(None) => Err(None),
                            Err(e) => Err(Some(e)),
                        }
                    })
                    .await;
                let id = match res {
                    Ok(id) => id,
                    Err(None) => return Ok(Lookup::NotRegistered),
                    Err(Some(e)) => return Err(e),
                };
                if let Some(c) = hub.try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(id.as_ref())) {
                    Ok(Lookup::Found(c))
                } else {
                    selector.reset().await;
                    Err("audit plugin instance has no ClientHub client".to_owned())
                }
            }
        }
    }
}
