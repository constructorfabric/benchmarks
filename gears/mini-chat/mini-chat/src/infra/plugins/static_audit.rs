//! `static-mini-chat-audit-plugin`: logs audit events.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

fn default_priority() -> i16 {
    100
}

fn default_enabled() -> bool {
    true
}

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticAuditConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default = "default_priority")]
    pub priority: i16,
}

/// Logging audit sink.
pub struct StaticAudit {
    enabled: bool,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAudit {
    async fn emit_audit_event(&self, event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        if self.enabled {
            tracing::info!(
                event_type = %event.event_type(),
                tenant_id = %event.tenant_id(),
                payload = %serde_json::to_string(&event).unwrap_or_default(),
                "mini-chat audit event"
            );
        }
        Ok(())
    }
}

/// Static audit plugin gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticMiniChatAuditPlugin;

#[async_trait]
impl Gear for StaticMiniChatAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let raw = ctx.raw_config();
        let cfg: StaticAuditConfig = if raw.as_object().is_none_or(serde_json::Map::is_empty) {
            StaticAuditConfig { enabled: true, vendor: default_vendor(), priority: default_priority() }
        } else {
            serde_json::from_value(raw.clone())?
        };
        let client: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(StaticAudit { enabled: cfg.enabled });
        let (instance_id, instance_json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_audit.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), client);
        Ok(())
    }
}
