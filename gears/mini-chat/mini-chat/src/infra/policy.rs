//! Model-policy and audit gateways: resolve the plugin instances through
//! types-registry (vendor + priority) and fetch their scoped clients from
//! ClientHub. The policy snapshot is not cached (ADR-0008).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, UserLimits,
};
use parking_lot_like::Mutex;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::errors::{DomainError, DomainResult};

mod parking_lot_like {
    pub type Mutex<T> = std::sync::Mutex<T>;
}

/// Resolves the model policy plugin.
pub struct PolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl PolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    async fn resolve_instance(&self) -> DomainResult<String> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::internal(format!("types-registry unavailable: {e}")))?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| DomainError::internal(format!("types-registry list failed: {e}")))?;
        choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| DomainError::internal(format!("model policy plugin: {e}")))
    }

    /// The plugin client.
    ///
    /// # Errors
    /// 500 internal when the plugin is not available.
    pub async fn client(&self) -> DomainResult<Arc<dyn MiniChatModelPolicyPluginClientV1>> {
        let id = self.selector.get_or_init(|| self.resolve_instance()).await?;
        self.hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(id.as_ref()))
            .ok_or_else(|| DomainError::internal("model policy plugin client not registered"))
    }

    /// Current policy snapshot for the user.
    ///
    /// # Errors
    /// 500 internal on plugin failure.
    pub async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<PolicySnapshot> {
        let c = self.client().await?;
        let v = c
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        c.get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Snapshot of a given version (settlement path).
    ///
    /// # Errors
    /// 500 internal on plugin failure.
    pub async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<PolicySnapshot> {
        let c = self.client().await?;
        c.get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Per-user limits.
    ///
    /// # Errors
    /// 500 internal on plugin failure.
    pub async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits> {
        let c = self.client().await?;
        c.get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("user limits: {e}")))
    }
}

/// Outcome of resolving the audit plugin.
pub enum AuditTarget {
    Client(Arc<dyn MiniChatAuditPluginClientV1>),
    /// No plugin registered: drop the event.
    NoPlugin,
    /// Transient resolution problem: retry.
    Retry(String),
}

/// Resolves the audit plugin. "No plugin" is not cached; a found instance
/// id is cached and reset when its client is missing from ClientHub.
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    cached: Mutex<Option<String>>,
    last_warn: Mutex<Option<Instant>>,
}

impl AuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            cached: Mutex::new(None),
            last_warn: Mutex::new(None),
        }
    }

    fn warn_no_plugin(&self) {
        let mut g = self.last_warn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let due = g.is_none_or(|t| t.elapsed() > Duration::from_secs(300));
        if due {
            tracing::warn!("no mini-chat audit plugin registered; audit events are dropped");
            *g = Some(Instant::now());
        }
    }

    pub async fn resolve(&self) -> AuditTarget {
        let cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let id = if let Some(id) = cached {
            id
        } else {
            let registry = match self.hub.get::<dyn TypesRegistryClient>() {
                Ok(r) => r,
                Err(e) => return AuditTarget::Retry(format!("types-registry unavailable: {e}")),
            };
            let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
            let instances = match registry
                .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
                .await
            {
                Ok(i) => i,
                Err(e) => return AuditTarget::Retry(format!("types-registry list failed: {e}")),
            };
            match choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
                &self.vendor,
                instances.iter().map(|e| (e.id.as_ref(), &e.object)),
            ) {
                Ok(id) => {
                    *self.cached.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id.clone());
                    id
                }
                Err(ChoosePluginError::PluginNotFound { .. }) => {
                    self.warn_no_plugin();
                    return AuditTarget::NoPlugin;
                }
                Err(e) => return AuditTarget::Retry(format!("audit plugin: {e}")),
            }
        };
        if let Some(c) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        {
            AuditTarget::Client(c)
        } else {
            *self.cached.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            AuditTarget::Retry("audit plugin client not registered".to_owned())
        }
    }
}
