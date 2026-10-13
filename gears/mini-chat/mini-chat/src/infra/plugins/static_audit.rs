//! `static-mini-chat-audit-plugin`: logs audit events.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use serde::Deserialize;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use toolkit::Gear;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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

/// Logging audit sink.
pub struct StaticAudit {
    enabled: bool,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for StaticAudit {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        if self.enabled {
            let json = serde_json::to_string(&event).unwrap_or_default();
            tracing::info!(target: "mini_chat::audit", event_type = %event.event_type(), event = %json, "audit event");
        }
        Ok(())
    }
}

/// Static audit plugin gear.
#[toolkit::gear(name = "static-mini-chat-audit-plugin", deps = [types_registry])]
pub struct StaticAuditPlugin {
    service: OnceLock<Arc<StaticAudit>>,
}

impl Default for StaticAuditPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticAuditPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticAuditConfig = ctx.config_or_default()?;
        let service = Arc::new(StaticAudit { enabled: cfg.enabled });
        let (instance_id, instance_json) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_audit.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatAuditPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}
