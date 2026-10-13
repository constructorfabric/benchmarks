//! Lazy resolution of the model policy and audit plugins through types-registry + ClientHub.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicySnapshot, PublishError, UsageEvent,
    UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::{AuditDelivery, AuditFailure, AuditPort, PolicyPort, PublishFailure};

const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);

enum Resolve {
    NotFound(String),
    Failed(String),
}

async fn resolve_instance<P>(hub: &ClientHub, vendor: &str, type_id: &str) -> Result<String, Resolve>
where
    P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
{
    let registry = hub
        .get::<dyn TypesRegistryClient>()
        .map_err(|e| Resolve::Failed(format!("types registry unavailable: {e}")))?;
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
        .await
        .map_err(|e| Resolve::Failed(format!("types registry list_instances failed: {e}")))?;
    choose_plugin_instance::<P>(vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object))).map_err(|e| match e {
        ChoosePluginError::PluginNotFound { .. } => Resolve::NotFound(format!("no plugin for vendor '{vendor}'")),
        ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
            Resolve::Failed(format!("invalid plugin instance '{gts_id}': {reason}"))
        }
    })
}

/// Production gateway to both plugins.
pub struct PluginGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    policy_selector: GtsPluginSelector,
    audit_selector: GtsPluginSelector,
    last_no_audit_warn: AtomicI64,
}

impl PluginGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            policy_selector: GtsPluginSelector::new(),
            audit_selector: GtsPluginSelector::new(),
            last_no_audit_warn: AtomicI64::new(0),
        }
    }

    async fn policy_plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id().to_string();
        let id = self
            .policy_selector
            .get_or_init(|| async {
                resolve_instance::<MiniChatModelPolicyPluginSpecV1>(&self.hub, &self.vendor, &type_id)
                    .await
                    .map_err(|e| match e {
                        Resolve::NotFound(m) | Resolve::Failed(m) => DomainError::internal(format!("model policy plugin: {m}")),
                    })
            })
            .await?;
        self.hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(id.as_ref()))
            .ok_or_else(|| DomainError::internal(format!("model policy plugin client not registered for '{id}'")))
    }
}

fn map_plugin_err(e: &mini_chat_sdk::PolicyPluginError) -> DomainError {
    DomainError::internal(format!("model policy plugin error: {e}"))
}

#[async_trait]
impl PolicyPort for PluginGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        let plugin = self.policy_plugin().await?;
        let v = plugin.get_current_policy_version(user_id).await.map_err(|e| map_plugin_err(&e))?;
        let s = plugin
            .get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| map_plugin_err(&e))?;
        Ok(Arc::new(s))
    }

    async fn snapshot_by_version(&self, user_id: Uuid, version: i64) -> Result<Arc<PolicySnapshot>, DomainError> {
        let plugin = self.policy_plugin().await?;
        let s = plugin.get_policy_snapshot(user_id, version).await.map_err(|e| map_plugin_err(&e))?;
        Ok(Arc::new(s))
    }

    async fn user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError> {
        let plugin = self.policy_plugin().await?;
        plugin.get_user_limits(user_id, version).await.map_err(|e| map_plugin_err(&e))
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishFailure> {
        let plugin = self.policy_plugin().await.map_err(|e| PublishFailure::Transient(e.to_string()))?;
        plugin.publish_usage(event).await.map_err(|e| match e {
            PublishError::Transient(m) => PublishFailure::Transient(m),
            PublishError::Permanent(m) => PublishFailure::Permanent(m),
        })
    }
}

#[async_trait]
impl AuditPort for PluginGateway {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<AuditDelivery, AuditFailure> {
        let type_id = MiniChatAuditPluginSpecV1::gts_type_id().to_string();
        // "No plugin" is not cached: get_or_init caches only Ok.
        let resolved = self
            .audit_selector
            .get_or_init(|| async {
                resolve_instance::<MiniChatAuditPluginSpecV1>(&self.hub, &self.vendor, &type_id).await
            })
            .await;
        let id = match resolved {
            Ok(id) => id,
            Err(Resolve::NotFound(m)) => {
                let now = time::OffsetDateTime::now_utc().unix_timestamp();
                let last = self.last_no_audit_warn.load(Ordering::Relaxed);
                if now - last >= 300 {
                    self.last_no_audit_warn.store(now, Ordering::Relaxed);
                    tracing::warn!(reason = %m, "no mini-chat audit plugin registered; audit events are dropped");
                }
                return Ok(AuditDelivery::NoPlugin);
            }
            Err(Resolve::Failed(m)) => return Err(AuditFailure::Transient(m)),
        };
        let Some(client) =
            self.hub.try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(id.as_ref()))
        else {
            self.audit_selector.reset().await;
            return Err(AuditFailure::Transient(format!("audit plugin client missing for '{id}'")));
        };
        match tokio::time::timeout(AUDIT_TIMEOUT, client.emit(event)).await {
            Err(_) | Ok(Err(AuditPluginError::PluginTimeout)) => Err(AuditFailure::Transient("audit plugin timeout".into())),
            Ok(Err(AuditPluginError::Transient(m))) => Err(AuditFailure::Transient(m)),
            Ok(Err(AuditPluginError::Permanent(m))) => Err(AuditFailure::Permanent(m)),
            Ok(Ok(())) => Ok(AuditDelivery::Delivered),
        }
    }
}
