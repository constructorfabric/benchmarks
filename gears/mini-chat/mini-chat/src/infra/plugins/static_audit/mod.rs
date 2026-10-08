//! Static audit plugin (`static-mini-chat-audit-plugin`): logs audit events.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StaticAuditConfig {
    pub enabled: bool,
    pub vendor: String,
    pub priority: i16,
}

impl Default for StaticAuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            vendor: "constructorfabric".to_owned(),
            priority: 100,
        }
    }
}

/// Logs every audit event when enabled.
pub struct StaticAuditService {
    enabled: bool,
}

impl StaticAuditService {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        if self.enabled {
            let json = serde_json::to_string(&event).unwrap_or_default();
            tracing::info!(event_type = %event.event_type(), tenant_id = %event.tenant_id(), event = %json, "mini-chat audit event");
        }
        Ok(())
    }
}

#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticAuditPlugin;

impl Default for StaticAuditPlugin {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        let (instance_id, instance_json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_audit.v1",
            &cfg.vendor,
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(StaticAuditService::new(cfg.enabled));
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        tracing::info!(instance_id = %instance_id, "static audit plugin registered");
        Ok(())
    }
}
