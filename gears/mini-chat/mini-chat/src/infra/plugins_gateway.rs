//! Model-policy and audit plugin gateways: plugin instances are resolved
//! lazily through types-registry and called through scoped `ClientHub`
//! clients.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1,
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicySnapshot,
    PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Error of a plugin resolution.
#[derive(Debug, Clone)]
pub enum ResolveError {
    /// No plugin instance registered for the vendor.
    NotRegistered,
    /// The instance resolved but its client is not in `ClientHub`.
    ClientMissing(String),
    /// Types-registry or other failure.
    Failed(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRegistered => write!(f, "no plugin registered"),
            Self::ClientMissing(id) => write!(f, "plugin client '{id}' not registered"),
            Self::Failed(m) => write!(f, "{m}"),
        }
    }
}

async fn list_instance<P>(hub: &ClientHub, vendor: &str) -> Result<String, ResolveError>
where
    P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
{
    let registry = hub
        .get::<dyn TypesRegistryClient>()
        .map_err(|e| ResolveError::Failed(format!("types-registry unavailable: {e}")))?;
    let type_id = <P as gts::GtsSchema>::TYPE_ID;
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
        .await
        .map_err(|e| ResolveError::Failed(format!("types-registry list failed: {e}")))?;
    choose_plugin_instance::<P>(vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object)))
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => ResolveError::NotRegistered,
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                ResolveError::Failed(format!("invalid plugin instance '{gts_id}': {reason}"))
            }
        })
}

/// Gateway to the model policy plugin.
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

    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, ResolveError> {
        let id = self
            .selector
            .get_or_init(|| {
                list_instance::<MiniChatModelPolicyPluginSpecV1>(&self.hub, &self.vendor)
            })
            .await?;
        if let Some(c) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(
                id.as_ref(),
            ))
        {
            Ok(c)
        } else {
            self.selector.reset().await;
            Err(ResolveError::ClientMissing(id.to_string()))
        }
    }

    async fn plugin_domain(
        &self,
    ) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        self.plugin()
            .await
            .map_err(|e| DomainError::internal(format!("model policy plugin: {e}")))
    }

    /// Current policy snapshot (version + snapshot).
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let p = self.plugin_domain().await?;
        let v = p
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        p.get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    /// Snapshot of a specific version (settlement).
    pub async fn snapshot(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        let p = self.plugin_domain().await?;
        p.get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))
    }

    pub async fn user_limits(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<UserLimits, DomainError> {
        let p = self.plugin_domain().await?;
        p.get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("user limits: {e}")))
    }

    /// Publish a usage event (usage outbox handler).
    pub async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError> {
        let p = self
            .plugin()
            .await
            .map_err(|e| PublishError::Transient(e.to_string()))?;
        p.publish_usage(ev).await
    }
}

/// Outcome of an audit delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDelivery {
    Ok,
    Dropped,
    Retry(String),
    Reject(String),
}

/// Gateway to the audit plugin. "No plugin registered" is not cached.
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    cached: Mutex<Option<String>>,
    last_missing_warning: Mutex<Option<Instant>>,
}

impl AuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            cached: Mutex::new(None),
            last_missing_warning: Mutex::new(None),
        }
    }

    pub async fn deliver(&self, event: AuditEvent) -> AuditDelivery {
        let cached = self.cached.lock().ok().and_then(|g| g.clone());
        let id = match cached {
            Some(id) => id,
            None => match list_instance::<MiniChatAuditPluginSpecV1>(&self.hub, &self.vendor).await
            {
                Ok(id) => {
                    if let Ok(mut g) = self.cached.lock() {
                        *g = Some(id.clone());
                    }
                    id
                }
                Err(ResolveError::NotRegistered) => {
                    let warn = if let Ok(mut g) = self.last_missing_warning.lock()
                        && g.is_none_or(|t| t.elapsed() > Duration::from_secs(300))
                    {
                        *g = Some(Instant::now());
                        true
                    } else {
                        false
                    };
                    if warn {
                        tracing::warn!(
                            "no mini-chat audit plugin registered; audit events are dropped"
                        );
                    }
                    return AuditDelivery::Dropped;
                }
                Err(e) => return AuditDelivery::Retry(e.to_string()),
            },
        };
        let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        else {
            if let Ok(mut g) = self.cached.lock() {
                *g = None;
            }
            return AuditDelivery::Retry(format!("audit plugin client '{id}' not registered"));
        };
        match tokio::time::timeout(Duration::from_secs(30), client.emit(event)).await {
            Ok(Ok(())) => AuditDelivery::Ok,
            Ok(Err(MiniChatAuditPluginError::Permanent(m))) => AuditDelivery::Reject(m),
            Ok(Err(e)) => AuditDelivery::Retry(e.to_string()),
            Err(_) => AuditDelivery::Retry(MiniChatAuditPluginError::PluginTimeout.to_string()),
        }
    }
}
