//! Gateways to the model policy and audit plugins (resolved lazily through
//! types-registry + scoped `ClientHub` registration).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, UserLimits,
};
use tokio::sync::RwLock;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::choose_plugin_instance;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Resolution failures (kept apart from "no plugin registered").
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginResolveError {
    #[error("no plugin instance registered")]
    NotRegistered,
    #[error("plugin resolution failed: {0}")]
    Failed(String),
    #[error("plugin client missing from ClientHub: {0}")]
    ClientMissing(String),
}

async fn resolve_instance<P>(hub: &ClientHub, vendor: &str) -> Result<String, PluginResolveError>
where
    P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
{
    let registry = hub
        .get::<dyn TypesRegistryClient>()
        .map_err(|e| PluginResolveError::Failed(e.to_string()))?;
    let type_id = P::TYPE_ID;
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
        .await
        .map_err(|e| PluginResolveError::Failed(e.to_string()))?;
    match choose_plugin_instance::<P>(vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))
    {
        Ok(id) => Ok(id),
        Err(toolkit::plugins::ChoosePluginError::PluginNotFound { .. }) => {
            Err(PluginResolveError::NotRegistered)
        }
        Err(e) => Err(PluginResolveError::Failed(e.to_string())),
    }
}

/// Model policy plugin gateway.
pub struct PolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    cached: RwLock<Option<String>>,
}

impl PolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            cached: RwLock::new(None),
        }
    }

    /// Resolve the plugin client.
    ///
    /// # Errors
    /// Resolution failure or missing client.
    pub async fn plugin(
        &self,
    ) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, PluginResolveError> {
        let cached = self.cached.read().await.clone();
        let id = if let Some(id) = cached {
            id
        } else {
            let id = resolve_instance::<MiniChatModelPolicyPluginSpecV1>(&self.hub, &self.vendor)
                .await?;
            *self.cached.write().await = Some(id.clone());
            id
        };
        if let Some(c) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(&id))
        {
            Ok(c)
        } else {
            *self.cached.write().await = None;
            Err(PluginResolveError::ClientMissing(id))
        }
    }

    async fn plugin_dom(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        self.plugin()
            .await
            .map_err(|e| DomainError::internal(format!("model policy plugin unavailable: {e}")))
    }

    /// Current policy snapshot for the user.
    ///
    /// # Errors
    /// 500 internal on plugin failure.
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let p = self.plugin_dom().await?;
        let v = p
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        p.get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Snapshot of a specific version (settlement).
    ///
    /// # Errors
    /// 500 internal on plugin failure.
    pub async fn snapshot(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        let p = self.plugin_dom().await?;
        p.get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Per-user limits.
    ///
    /// # Errors
    /// 500 internal on plugin failure.
    pub async fn user_limits(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<UserLimits, DomainError> {
        let p = self.plugin_dom().await?;
        p.get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("user limits: {e}")))
    }
}

/// Audit plugin gateway. "No plugin registered" is never cached.
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    cached: RwLock<Option<String>>,
    last_missing_warn: AtomicU64,
}

impl AuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            cached: RwLock::new(None),
            last_missing_warn: AtomicU64::new(0),
        }
    }

    /// `Ok(None)` when no audit plugin is registered.
    ///
    /// # Errors
    /// Resolution failure or a resolved instance without a client.
    pub async fn plugin(
        &self,
    ) -> Result<Option<Arc<dyn MiniChatAuditPluginClientV1>>, PluginResolveError> {
        let cached = self.cached.read().await.clone();
        let id = if let Some(id) = cached {
            id
        } else {
            match resolve_instance::<MiniChatAuditPluginSpecV1>(&self.hub, &self.vendor).await {
                Ok(id) => {
                    *self.cached.write().await = Some(id.clone());
                    id
                }
                Err(PluginResolveError::NotRegistered) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or_default();
                    let last = self.last_missing_warn.load(Ordering::Relaxed);
                    if now.saturating_sub(last) >= 300 {
                        self.last_missing_warn.store(now, Ordering::Relaxed);
                        tracing::warn!(
                            "no mini-chat audit plugin registered; audit events are dropped"
                        );
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        };
        if let Some(c) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        {
            Ok(Some(c))
        } else {
            *self.cached.write().await = None;
            Err(PluginResolveError::ClientMissing(id))
        }
    }
}
