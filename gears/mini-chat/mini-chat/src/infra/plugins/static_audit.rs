//! Bundled static audit plugin (`static-mini-chat-audit-plugin`): logs audit
//! events delivered by the audit outbox handler.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError,
    MiniChatAuditPluginSpecV1,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
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
            vendor: crate::config::DEFAULT_VENDOR.to_owned(),
            priority: 100,
        }
    }
}

pub struct StaticAuditService {
    enabled: bool,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAuditService {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            let json = serde_json::to_string(&event).unwrap_or_default();
            info!(target: "mini_chat.audit", event_type = %event.event_type(), event = %json, "audit event");
        }
        Ok(())
    }
}

/// Static audit plugin gear.
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
        let (instance_id, instance_json) =
            PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
                "cf.builtin.static_mini_chat_audit.plugin.v1",
                &cfg.vendor,
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let svc: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(StaticAuditService {
            enabled: cfg.enabled,
        });
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                svc,
            );
        info!(instance_id = %instance_id, enabled = cfg.enabled, "static mini-chat audit plugin registered");
        Ok(())
    }
}
